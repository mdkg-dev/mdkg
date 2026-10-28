//! Write commands: `new`, `done`, `update`, `next`,
//! `goal` and `format`.
//!
//! Lasting statuses (backlog, todo, done) are written to node files;
//! temporary ones (progress, review, blocked) stay in the coordinator, so
//! they never become commits. Edits to tracked nodes happen under a lease and
//! are recorded as a changeset with the git commit that holds them.

use std::collections::BTreeMap;
use std::path::Path;

use mdkg_coord::{Changes, Command, Outcome, Scope, Settlement};
use mdkg_coord_sqlite::{Store, StoreError, git};
use mdkg_format::NodeId;
use mdkg_format::frontmatter::{self, Frontmatter, Value as FmValue};
use mdkg_format::node::{FormatVersion, WORK_TYPES};
use mdkg_format::{Code, Diagnostic, Severity};
use mdkg_graph::graph_checks::goal_scope;
use mdkg_graph::resolve::{Resolution, resolve_error, resolve_qid};
use mdkg_graph::{Config, ErrorKind, GraphError, Index, IndexNode, config, index, templates};
use mdkg_index::resolve::{describe, handle_error, resolve_handle, resolve_noted};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::coord::{
    self, LeaseRecord, Repo, View, git_error, lease_record, now_ms, refusal, store_error,
};
use crate::output::{self, NodeRecord, Report, rfc3339};
use crate::read::{effective_or_fail, effective_or_warn, records};
use mdkg_format::program::BIN;

const DESIGN_TYPES: &[&str] = &["edd", "dec", "prd", "prop"];
const CORE_TYPES: &[&str] = &["rule"];
const NEXT_TYPES: &[&str] = &["feat", "task", "bug", "test", "spike"];
/// Lasting statuses stored in node files.
const LASTING: &[&str] = &["backlog", "todo", "done"];
/// Temporary states `update` records in the coordinator.
const TEMPORARY: &[&str] = &["review", "blocked"];
const CHECKPOINT_KINDS: &[&str] = &[
    "implementation",
    "test-proof",
    "goal-closeout",
    "audit",
    "handoff",
];

/// `updated` (and `created`) for an edit now: in a format 0.7 graph an RFC
/// 3339 UTC timestamp to the millisecond, otherwise the local date, as
/// 0.6.0 writes it.
pub(crate) fn today(repo: &Repo, now: u64) -> String {
    if config::load(&repo.root).is_ok_and(|c| c.format == FormatVersion::V07) {
        return timestamp(now);
    }
    let offset_ms = git::local_offset_minutes(&repo.top) * 60_000;
    let local = i128::from(now) + i128::from(offset_ms);
    rfc3339(u64::try_from(local).unwrap_or_default())[..10].to_string()
}

/// `updated` never before `created` or the last `updated` (format 0.7,
/// section 3.4): a new node's `created` is its UUID's time, which may run a
/// little ahead of the clock, and the clock may step back.
pub(crate) fn not_before(stamp: &str, floors: &[&str]) -> String {
    use mdkg_format::node::{is_date, is_timestamp};
    if !is_timestamp(stamp) {
        return stamp.to_string();
    }
    // A date reads as its first millisecond, UTC.
    floors
        .iter()
        .filter_map(|f| {
            if is_timestamp(f) {
                Some((*f).to_string())
            } else if is_date(f) {
                Some(format!("{f}T00:00:00.000Z"))
            } else {
                None
            }
        })
        .fold(stamp.to_string(), |a, b| if b > a { b } else { a })
}

/// How a graph's node files are trimmed: format 0.7's ASCII rule, or
/// 0.6.0's (format 0.7, section 3.3).
pub(crate) fn lexical(config: &Config) -> frontmatter::Lexical {
    if config.format == FormatVersion::V07 {
        frontmatter::Lexical::Ascii
    } else {
        frontmatter::Lexical::Js
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (format 0.7, section 3.4).
pub fn timestamp(ms: u64) -> String {
    format!("{}.{:03}Z", &rfc3339(ms)[..19], ms % 1000)
}

/// `slugifyTitle` from 0.6.0.
fn slugify(title: &str) -> String {
    let lowered = title.trim().to_lowercase();
    let mut slug = String::new();
    for c in lowered.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        return "node".into();
    }
    if slug.len() > 80 {
        return slug[..80].trim_end_matches('-').to_string();
    }
    slug
}

fn prefix_for(node_type: &str) -> &str {
    if node_type == "checkpoint" {
        "chk"
    } else {
        node_type
    }
}

fn folder_for(node_type: &str) -> Option<&'static str> {
    if CORE_TYPES.contains(&node_type) {
        Some("core")
    } else if DESIGN_TYPES.contains(&node_type) {
        Some("design")
    } else if WORK_TYPES.contains(&node_type) {
        Some("work")
    } else {
        None
    }
}

fn list(values: &[String]) -> Option<FmValue> {
    let items: Vec<String> = values
        .iter()
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .collect();
    (!items.is_empty()).then_some(FmValue::List(items))
}

fn is_placeholder(value: &str) -> Option<&str> {
    let inner = value.strip_prefix("{{")?.strip_suffix("}}")?;
    inner
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        .then_some(inner)
}

/// `renderTemplate` from 0.6.0: placeholders are filled from `context` or
/// dropped, literal values stay, and keys come out in the default order.
fn render(template: &str, context: &BTreeMap<&str, FmValue>) -> Result<String, GraphError> {
    let parsed = frontmatter::parse(template, "<template>")
        .map_err(|e| GraphError::from_format(&e, Path::new("")))?;
    let mut fm = Frontmatter::default();
    for (key, value) in parsed.frontmatter.entries() {
        // A legacy_id names the one node that had that 0.6.0 id, so a
        // template copied from a migrated node never passes it on (RC1
        // audit I2).
        if key == "legacy_id" {
            continue;
        }
        match value {
            FmValue::String(s) if is_placeholder(s).is_some() => {
                if let Some(v) = is_placeholder(s).and_then(|t| context.get(t)) {
                    fm.insert(key.clone(), v.clone());
                }
            }
            other => fm.insert(key.clone(), other.clone()),
        }
    }
    for (key, value) in context {
        if parsed.frontmatter.contains(key) {
            fm.insert(*key, value.clone());
        }
    }
    let mut body = parsed.body.trim_start().to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    Ok(frontmatter::write(&fm, &body))
}

/// What a new node needs beyond its type and title.
#[derive(Debug, Default)]
pub struct NewSpec {
    pub node_type: String,
    pub title: String,
    pub status: Option<String>,
    pub priority: Option<i64>,
    pub parent: Option<String>,
    pub epic: Option<String>,
    pub prev: Option<String>,
    pub next: Option<String>,
    pub tags: Vec<String>,
    pub owners: Vec<String>,
    pub refs: Vec<String>,
    pub relates: Vec<String>,
    pub blocked_by: Vec<String>,
    pub blocks: Vec<String>,
    pub skills: Vec<String>,
    pub scope: Vec<String>,
    pub kind: Option<String>,
}

fn bad(code: Code, message: impl Into<String>) -> GraphError {
    GraphError::usage(code, message)
}

/// Checks a spec and fills defaults; returns the frontmatter context.
fn context_for<'a>(
    spec: &'a NewSpec,
    config: &Config,
    index: &Index,
    id: &'a str,
    date: &str,
) -> Result<BTreeMap<&'a str, FmValue>, GraphError> {
    let t = spec.node_type.as_str();
    let work = WORK_TYPES.contains(&t);
    let mut ctx: BTreeMap<&str, FmValue> = BTreeMap::new();
    let s = |v: &str| FmValue::String(v.to_string());
    ctx.insert("id", s(id));
    ctx.insert("type", s(t));
    ctx.insert("title", s(&spec.title));
    ctx.insert("created", s(date));
    ctx.insert("updated", s(date));
    let status = spec.status.as_deref().map(str::to_lowercase);
    if work {
        let default = match t {
            "checkpoint" => "done",
            "goal" => "todo",
            _ => config.status_enum.first().map_or("backlog", String::as_str),
        };
        let status = status.unwrap_or_else(|| default.to_string());
        if !LASTING.contains(&status.as_str()) {
            return Err(bad(
                Code::UsageBadStatus,
                format!("status {status} isn't a lasting status"),
            )
            .with_hint("files keep backlog, todo or done; claim work for progress, and use `update` for review or blocked"));
        }
        ctx.insert("status", s(&status));
        let priority = spec.priority.unwrap_or(config.priority_max);
        if priority < config.priority_min || priority > config.priority_max {
            return Err(bad(
                Code::NodeBadPriority,
                format!(
                    "priority must be between {} and {}",
                    config.priority_min, config.priority_max
                ),
            ));
        }
        ctx.insert("priority", s(&priority.to_string()));
    } else if t == "dec" {
        let status = status.unwrap_or_else(|| "proposed".into());
        if !mdkg_format::node::DEC_STATUS.contains(&status.as_str()) {
            return Err(bad(
                Code::UsageBadStatus,
                format!(
                    "status must be one of {}",
                    mdkg_format::node::DEC_STATUS.join(", ")
                ),
            ));
        }
        ctx.insert("status", s(&status));
    } else if status.is_some() {
        return Err(bad(
            Code::UsageBadValue,
            "--status is only for work items and decisions",
        ));
    }
    if !work && spec.priority.is_some() {
        return Err(bad(
            Code::UsageBadValue,
            "--priority is only for work items",
        ));
    }
    for (key, value) in [
        ("parent", &spec.parent),
        ("epic", &spec.epic),
        ("prev", &spec.prev),
        ("next", &spec.next),
    ] {
        if let Some(v) = value {
            if !work {
                return Err(bad(
                    Code::UsageBadValue,
                    format!("--{key} is only for work items"),
                ));
            }
            let v = v.trim().to_string();
            let (target, written) =
                edge_value(index, config.format == FormatVersion::V07, key, &v)?;
            // A chain edge needs the target's edge back to the new node,
            // which can't exist yet unless the target already names it.
            let back = index.get(&target).and_then(|n| match key {
                "prev" => n.edges.next.clone(),
                "next" => n.edges.prev.clone(),
                _ => Some(String::new()),
            });
            let names_us = back
                .as_deref()
                .is_some_and(|b| b == id || b.ends_with(&format!(":{id}")));
            if matches!(key, "prev" | "next") && !names_us {
                let opposite = if key == "prev" { "next" } else { "prev" };
                return Err(GraphError::new(
                    ErrorKind::Validation,
                    Diagnostic::error(
                        Code::EdgePrevNextAsymmetric,
                        format!("{v} has no {opposite} edge back to the new node"),
                    )
                    .with_hint(format!(
                        "create the node first, then add `{key}` here and `{opposite}` on {v}"
                    )),
                ));
            }
            // Format 0.7 refs are full ids, whatever handle named them.
            ctx.insert(key, s(&written));
        }
    }
    for (key, values) in [
        ("relates", &spec.relates),
        ("blocked_by", &spec.blocked_by),
        ("blocks", &spec.blocks),
    ] {
        if let Some(FmValue::List(items)) = list(values) {
            let mut resolved = Vec::with_capacity(items.len());
            for v in items {
                let (_, written) = edge_value(index, config.format == FormatVersion::V07, key, &v)?;
                resolved.push(written);
            }
            ctx.insert(key, FmValue::List(resolved));
        }
    }
    for (key, values) in [
        ("tags", &spec.tags),
        ("owners", &spec.owners),
        ("skills", &spec.skills),
    ] {
        if let Some(v) = list(values) {
            ctx.insert(key, v);
        }
    }
    // Node refs given as handles are written as full ids in a format 0.7
    // graph: scope names nodes only, refs may also hold URIs.
    for (key, values) in [("refs", &spec.refs), ("scope", &spec.scope)] {
        let Some(FmValue::List(items)) = list(values) else {
            continue;
        };
        let v07 = config.format == FormatVersion::V07;
        let mut resolved = Vec::with_capacity(items.len());
        for v in items {
            // A written ref keeps its graph and pin; only the id is made
            // current. A handle becomes the node's full id.
            if v07 && let Ok(r) = mdkg_format::NodeRef::parse(&v) {
                match index.target(&v) {
                    mdkg_graph::index::RefTarget::Local { node, .. } => {
                        let id = NodeId::parse(&node.id).unwrap_or(r.id.clone());
                        resolved.push(mdkg_format::NodeRef { id, ..r }.to_string());
                        continue;
                    }
                    mdkg_graph::index::RefTarget::Foreign if key == "refs" => {
                        resolved.push(v);
                        continue;
                    }
                    _ => return Err(no_match(key)),
                }
            }
            // An external URI in refs stays as written (by the URI grammar,
            // so a title holding `://` is still a handle).
            if key == "refs" && mdkg_format::ids::is_uri_ref(&v) {
                resolved.push(v);
                continue;
            }
            // Any other value names a node by a handle, in either format;
            // a ref that names none stays as written, but scope names
            // nodes only. All before anything is allocated or written.
            match resolve_handle(index, &v, None) {
                Resolution::Ok(_) => {
                    let (qid, written) = edge_value(index, v07, key, &v)?;
                    // 0.6.0's scope takes bare ids only.
                    resolved.push(match index.get(&qid) {
                        Some(n) if key == "scope" && !v07 => n.id.clone(),
                        _ => written,
                    });
                }
                other @ Resolution::Ambiguous(_) => {
                    return Err(handle_error(index, key, &v, &other, None));
                }
                _ if key == "scope" => return Err(no_match(key)),
                _ => resolved.push(v),
            }
        }
        ctx.insert(key, FmValue::List(resolved));
    }
    if t == "checkpoint" {
        let kind = spec.kind.clone().unwrap_or_else(|| "implementation".into());
        if !CHECKPOINT_KINDS.contains(&kind.as_str()) {
            return Err(bad(
                Code::UsageBadValue,
                format!(
                    "checkpoint kind must be one of {}",
                    CHECKPOINT_KINDS.join(", ")
                ),
            ));
        }
        ctx.insert("checkpoint_kind", s(&kind));
    }
    if t == "goal" {
        let active = index.nodes.iter().any(|n| {
            n.node_type == "goal"
                && n.attributes.get("goal_state").and_then(|v| v.as_str()) == Some("active")
        });
        ctx.insert("goal_state", s(if active { "paused" } else { "active" }));
        ctx.insert("goal_condition", s(&spec.title));
        ctx.insert("max_iterations", s("25"));
        ctx.insert("blocked_after_attempts", s("3"));
    }
    Ok(ctx)
}

/// The highest `<prefix>-<n>` in the worktree's index.
fn local_floor(index: &Index, prefix: &str) -> u64 {
    index
        .nodes
        .iter()
        .filter_map(|n| {
            n.id.strip_prefix(prefix)?
                .strip_prefix('-')?
                .parse::<u64>()
                .ok()
        })
        .max()
        .unwrap_or(0)
}

/// Creates a node file and returns (id, graph-relative path).
fn create(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    spec: &NewSpec,
    now: u64,
) -> Result<(String, String), GraphError> {
    let t = spec.node_type.as_str();
    // A name that isn't a type is usage.unknown_type (D2); a real type
    // `new` can't make is usage.unsupported_type, and may be named.
    let folder = folder_for(t).ok_or_else(|| {
        let creates =
            "task, bug, feat, spike, test, epic, goal, checkpoint, edd, dec, prd, prop, rule";
        if mdkg_format::node::ALLOWED_TYPES.contains(&t) {
            bad(
                Code::UsageUnsupportedType,
                format!("`new` doesn't create {t} nodes yet"),
            )
            .with_hint(format!("types `new` creates: {creates}"))
        } else {
            bad(Code::UsageUnknownType, "that isn't a node type")
                .with_hint(format!("types `new` creates: {creates}"))
        }
    })?;
    let config = config::load(&repo.root)?;
    let index = index::build(&repo.root, &config)?;
    if config.format == FormatVersion::V07 {
        return create_v07(repo, store, actor, spec, &config, &index, folder);
    }
    let prefix = prefix_for(t).to_string();
    let (template, _bundled) = templates::template_text(&repo.root, &config, t)?;
    // Check the spec with a stand-in id first, so a bad request uses up no
    // alias.
    let stand_in = format!("{prefix}-0");
    render(
        &template,
        &context_for(spec, &config, &index, &stand_in, &today(repo, now))?,
    )?;
    // Before an alias is used up: never through a linked folder (SEC3).
    crate::anchor::refuse_linked_folders(&repo.root, &format!(".mdkg/{folder}/new.md"))?;
    let floor = local_floor(&index, &prefix);
    let alias = match store
        .execute(&Command::IssueAlias {
            prefix: prefix.clone(),
            node: format!("{actor}@{now}"),
            floor,
        })
        .map_err(store_error)?
    {
        Outcome::Alias { alias } => alias,
        other => {
            return Err(GraphError::failure(
                Code::CoordStoreFailed,
                format!("unexpected outcome {other:?}"),
            ));
        }
    };
    let ctx = context_for(spec, &config, &index, &alias, &today(repo, now))?;
    let content = render(&template, &ctx)?;
    let rel = format!(".mdkg/{folder}/{alias}-{}.md", slugify(&spec.title));
    let path = repo.root.join(&rel);
    // A dangling link is there too, and is never written through (RC1
    // audit SEC8).
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err(GraphError::failure(
            Code::NodeExists,
            format!("{rel} already exists"),
        ));
    }
    // Never inside another actor's exclusive lease on the new file or a
    // folder holding it. With a lease authority those leases
    // are there, not here: this sees only local ones.
    let leaf = repo.leaf(&rel)?;
    coord::check_scope_text(&leaf)?;
    if let Some(h) = store
        .coordinator()
        .map_err(store_error)?
        .holders(&leaf)
        .find(|l| l.owner != actor && l.mode == mdkg_coord::Mode::Exclusive && l.deadline > now)
    {
        return Err(GraphError::new(
            ErrorKind::Conflict,
            Diagnostic::error(
                Code::CoordLeaseHeld,
                format!(
                    "the new node's file is held by another lease until {} ({})",
                    rfc3339(h.deadline),
                    h.id
                ),
            )
            .at(h.scope.to_string(), None)
            .held_by([h.owner.clone()])
            .with_hint("pick other work, or wait for the lease to end"),
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| {
            GraphError::failure(
                Code::IoWriteFailed,
                format!("can't create {}: {e}", dir.display()),
            )
        })?;
    }
    // Created, never opened: a link that appeared meanwhile isn't followed.
    write_synced(&path, content.as_bytes())
        .map_err(|e| GraphError::failure(Code::IoWriteFailed, format!("can't write {rel}: {e}")))?;
    // The new file must leave the graph valid; undo it if not.
    if let Err(e) = index::build(&repo.root, &config) {
        std::fs::remove_file(&path).ok();
        return Err(e);
    }
    Ok((alias, rel))
}

/// `new` in a format 0.7 graph: a minted id, no alias and no counter, and
/// the file `<type>-<slug>.md`, with `-2`, `-3`… on a clash.
///
/// The node and the graph with it are checked in memory first. The file is
/// then written whole to a temporary name and linked into place, which
/// fails rather than replace a file that appeared meanwhile, so a crash or
/// a race never leaves a partial, overwritten or invalid node.
fn create_v07(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    spec: &NewSpec,
    config: &Config,
    index: &Index,
    folder: &str,
) -> Result<(String, String), GraphError> {
    let t = spec.node_type.as_str();
    let id = NodeId::mint(t).map_err(|e| bad(Code::UsageUnsupportedType, e.to_string()))?;
    // Minted ids are UUIDv7s, so the time is always there.
    let stamp = timestamp(id.uuid().unix_ms().unwrap_or_else(now_ms));
    let (template, _bundled) = templates::template_text(&repo.root, config, t)?;
    let id_text = id.to_string();
    let ctx = context_for(spec, config, index, &id_text, &stamp)?;
    let content = render(&template, &ctx)?;
    let stem = format!("{t}-{}", slugify(&spec.title));
    // Profile C (`work.committed: none`): work-type nodes go
    // beside the coordinator store, which git never sees and every
    // worktree reads. Creators there take turns: each re-reads the graph
    // under the lock and checks before publishing, so no node another
    // worktree may already depend on is ever taken back.
    let in_store = config.work_committed == mdkg_graph::config::WorkCommitted::None
        && mdkg_graph::store_work::STORE_WORK_TYPES.contains(&t);
    let store_dir = in_store
        .then(|| {
            config.store_work_dir.clone().ok_or_else(|| {
                GraphError::failure(
                    Code::IoGitFailed,
                    "work.committed \"none\" keeps work nodes beside the coordinator store, \
                     which needs a git repository",
                )
            })
        })
        .transpose()?;
    let _turn = store_dir.as_deref().map(store_work_lock).transpose()?;
    let fresh;
    let index = if in_store {
        fresh = index::build(&repo.root, config)?;
        &fresh
    } else {
        index
    };
    check_new_in_memory(
        repo,
        config,
        index,
        folder,
        store_dir.as_deref(),
        &stem,
        &content,
    )?;
    let dir = match &store_dir {
        Some(d) => d.clone(),
        None => repo.root.join(".mdkg").join(folder),
    };
    let base = match &store_dir {
        Some(d) => store_base(d),
        None => {
            // Never through a linked folder out of the repository (RC1
            // audit SEC3); the store's folder is in the git folder.
            crate::anchor::refuse_linked_folders(&repo.root, &format!(".mdkg/{folder}/{stem}.md"))?;
            repo.root.as_path()
        }
    };
    create_dir_durably(&dir, base).map_err(|e| {
        GraphError::failure(
            Code::IoWriteFailed,
            format!("can't create {}: {e}", dir.display()),
        )
    })?;
    sweep_new_temps(&dir);
    let tmp = TempFile(dir.join(format!("{NEW_TEMP_PREFIX}{}.tmp", id.uuid())));
    // The whole file is on disk before it gets its name: profile C keeps
    // no other copy of it (RC1 audit O1).
    write_synced(&tmp.0, content.as_bytes())
        .map_err(|e| {
            GraphError::failure(
                Code::IoWriteFailed,
                format!("can't write .mdkg/{folder}: {e}"),
            )
        })
        .and_then(|()| fault("staged"))?;
    let rel = if in_store {
        place_in_store(&tmp.0, &dir, &stem)?
    } else {
        place_new(repo, store, actor, &tmp.0, folder, &stem)?
    };
    drop(tmp);
    // And its name is, before `new` reports it.
    let published = sync_dir(&dir)
        .map_err(|e| {
            GraphError::failure(
                Code::IoWriteFailed,
                format!("can't sync {}: {e}", dir.display()),
            )
        })
        .and_then(|()| fault("linked"));
    if in_store {
        // Checked under the lock already; once published, another
        // worktree may refer to it, so it stays.
        return match published {
            Ok(()) => Ok((id.to_string(), rel)),
            Err(e) => Err(e.with_hint(format!(
                "{id} is at {rel} and stays there, but a crash may still lose it; \
                 check that it's there after a restart"
            ))),
        };
    }
    // Checked in memory already; this is the last guard against a graph
    // that changed meanwhile.
    if let Err(e) = published.and_then(|()| index::build(&repo.root, config).map(drop)) {
        return Err(match std::fs::remove_file(repo.root.join(&rel)) {
            Ok(()) => e,
            Err(io) => e.with_hint(format!("the new file {rel} couldn't be removed: {io}")),
        });
    }
    Ok((id.to_string(), rel))
}

/// Writes a new file and syncs it; an existing one is never replaced.
fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Syncs a folder, so the names just made or removed in it survive a crash.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// `create_dir_all`, then syncs every folder from `dir`'s parent up to
/// `base`, so `dir` survives a crash. Whoever made a folder: another
/// creator may have made it and not synced it yet (Codex, 4fea836).
fn create_dir_durably(dir: &Path, base: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut at = dir;
    while at != base {
        let Some(parent) = at.parent() else { break };
        sync_dir(parent)?;
        at = parent;
    }
    Ok(())
}

/// The folder a profile C work folder (`<common>/mdkg/work/<key>`) is
/// made under: the git common folder, which git keeps.
fn store_base(dir: &Path) -> &Path {
    dir.ancestors().nth(3).unwrap_or(dir)
}

/// The temporary names `new` writes nodes under before linking them.
const NEW_TEMP_PREFIX: &str = ".mdkg-new-";

/// A temporary file, removed when dropped (every error path included).
struct TempFile(std::path::PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// Removes `new`'s temporary files left by a crash: only mdkg's own names,
/// and only ones older than ten minutes, so a running `new` keeps its own.
fn sweep_new_temps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = (name.starts_with(NEW_TEMP_PREFIX) || name.starts_with(".mdkg-write-"))
            && name.ends_with(".tmp");
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < cutoff);
        if ours && old && entry.file_type().is_ok_and(|t| t.is_file()) {
            std::fs::remove_file(entry.path()).ok();
        }
    }
}

/// The new node, and the graph with it, as `index::build` would check them.
fn check_new_in_memory(
    repo: &Repo,
    config: &Config,
    index: &Index,
    folder: &str,
    store_dir: Option<&Path>,
    stem: &str,
    content: &str,
) -> Result<(), GraphError> {
    // Where the node will be: profile C's store folder keeps its absolute
    // path, as the index does.
    let rel = match store_dir {
        Some(dir) => dir
            .join(format!("{stem}.md"))
            .to_string_lossy()
            .into_owned(),
        None => format!(".mdkg/{folder}/{stem}.md"),
    };
    // The index limits too, so a node that would put the graph over them is
    // never placed.
    let limits = &config.limits;
    let documents = mdkg_graph::discovery::discover(&repo.root, config)?;
    let (mut files, mut bytes) = (1u64, content.len() as u64);
    for (_, docs) in &documents.by_workspace {
        for f in docs {
            files += 1;
            bytes += std::fs::metadata(f).map_or(0, |m| m.len());
        }
    }
    let over = if content.len() as u64 > limits.max_file_bytes {
        Some("index.limits.max_file_bytes")
    } else if files > limits.max_files {
        Some("index.limits.max_files")
    } else if bytes > limits.max_total_bytes {
        Some("index.limits.max_total_bytes")
    } else {
        None
    };
    if let Some(limit) = over {
        return Err(GraphError::failure(
            Code::IndexLimitExceeded,
            format!("the new node would exceed {limit}"),
        ));
    }
    let schemas = templates::load(&repo.root, config)?.schemas;
    let opts = mdkg_format::node::ParseOptions {
        work_status: &config.status_enum,
        priority_min: config.priority_min,
        priority_max: config.priority_max,
        templates: &schemas,
        allow_extension_keys: true,
        format: config.format,
    };
    let node = mdkg_format::node::parse_node(content, &rel, &opts)
        .map_err(|e| GraphError::from_format(&e, &repo.root))?;
    // The workspace at the repository root, which `.mdkg/` belongs to.
    let ws = config
        .workspaces
        .iter()
        .find(|w| w.enabled && matches!(w.path.trim_end_matches('/'), "." | "" | "./."))
        .map_or("root", |w| w.alias.as_str());
    // What index::build refuses of a single node, so a profile C node,
    // which can't be taken back once published, never breaks the graph
    // (RC1 audit I2).
    if node.identity.is_some() {
        return Err(GraphError::failure(
            Code::NodeIdentityUnsupported,
            "identity-bearing node requires an explicit v2 graph manifest; no implicit migration",
        )
        .at(rel, None)
        .with_hint("remove graph_id and node_id from the template"));
    }
    if let Some(legacy) = &node.legacy_id
        && !config.tolerant
        && let Some(first) = index
            .nodes
            .iter()
            .find(|n| n.ws == ws && n.legacy_id.as_deref() == Some(legacy.as_str()))
    {
        return Err(GraphError::new(
            ErrorKind::Failure,
            mdkg_graph::index::duplicate_legacy_id(legacy, ws, &node.id, &rel, &first.path),
        ));
    }
    let qid = format!("{ws}:{}", node.id);
    let mut nodes = index.nodes.clone();
    nodes.push(IndexNode::from_parsed(node, ws, rel));
    let mut preview = Index::assemble_in(nodes, index.workspaces.clone(), config.graph_id);
    preview.private_work = index.private_work;
    let external: std::collections::HashSet<&str> =
        config.subgraphs.iter().map(String::as_str).collect();
    // And, in profile C, no committed node of its naming a work node git
    // never holds (RC1 audit E1).
    let private = mdkg_graph::graph_checks::private_targets(&preview)
        .into_iter()
        .filter(|d| d.node.as_deref() == Some(qid.as_str()));
    match mdkg_graph::graph_checks::collect(&preview, config.tolerant, &external)
        .into_iter()
        .chain(private)
        .next()
    {
        Some(first) => Err(GraphError::new(ErrorKind::Validation, first)),
        None => Ok(()),
    }
}

/// Links `tmp` into `.mdkg/<folder>/` as `<stem>.md`, or `<stem>-2.md` and
/// so on when the name is taken, and returns the graph-relative path.
fn place_new(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    tmp: &Path,
    folder: &str,
    stem: &str,
) -> Result<String, GraphError> {
    for n in 1..=10_000u32 {
        let name = if n == 1 {
            format!("{stem}.md")
        } else {
            format!("{stem}-{n}.md")
        };
        let rel = format!(".mdkg/{folder}/{name}");
        let path = repo.root.join(&rel);
        if path.exists() {
            continue;
        }
        check_new_file_lease(repo, store, actor, &rel)?;
        match std::fs::hard_link(tmp, &path) {
            Ok(()) => return Ok(rel),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(GraphError::failure(
                    Code::IoWriteFailed,
                    format!("can't write {rel}: {e}"),
                ));
            }
        }
    }
    Err(GraphError::failure(
        Code::NodeExists,
        format!("every name for .mdkg/{folder}/{stem}.md is taken"),
    ))
}

/// The lock profile C's creators take turns on: a file in the store's
/// work folder, held exclusively until the returned handle drops.
pub(crate) fn store_work_lock(dir: &Path) -> Result<std::fs::File, GraphError> {
    let fail = |e: std::io::Error| {
        GraphError::failure(
            Code::IoWriteFailed,
            format!("can't lock {}: {e}", dir.display()),
        )
    };
    create_dir_durably(dir, store_base(dir)).map_err(fail)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".mdkg-new.lock"))
        .map_err(fail)?;
    file.lock().map_err(fail)?;
    Ok(file)
}

/// Links the new node's file into the store's work folder (profile C)
/// under the first free name, and returns its path, absolute: it is
/// outside every worktree, so no path lease covers it.
fn place_in_store(tmp: &Path, dir: &Path, stem: &str) -> Result<String, GraphError> {
    for n in 1..=10_000u32 {
        let name = if n == 1 {
            format!("{stem}.md")
        } else {
            format!("{stem}-{n}.md")
        };
        let path = dir.join(&name);
        match std::fs::hard_link(tmp, &path) {
            Ok(()) => return Ok(path.to_string_lossy().into_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(GraphError::failure(
                    Code::IoWriteFailed,
                    format!("can't write {}: {e}", path.display()),
                ));
            }
        }
    }
    Err(GraphError::failure(
        Code::NodeExists,
        format!("every name for {stem}.md in {} is taken", dir.display()),
    ))
}

/// Never inside another actor's exclusive lease on the new file or a folder
/// holding it. With a lease authority those leases are there,
/// not here: this sees only local ones.
fn check_new_file_lease(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    rel: &str,
) -> Result<(), GraphError> {
    let now = now_ms();
    let leaf = repo.leaf(rel)?;
    coord::check_scope_text(&leaf)?;
    if let Some(h) = store
        .coordinator()
        .map_err(store_error)?
        .holders(&leaf)
        .find(|l| l.owner != actor && l.mode == mdkg_coord::Mode::Exclusive && l.deadline > now)
    {
        return Err(GraphError::new(
            ErrorKind::Conflict,
            Diagnostic::error(
                Code::CoordLeaseHeld,
                format!(
                    "the new node's file is held by another lease until {} ({})",
                    rfc3339(h.deadline),
                    h.id
                ),
            )
            .at(h.scope.to_string(), None)
            .held_by([h.owner.clone()])
            .with_hint("pick other work, or wait for the lease to end"),
        ));
    }
    Ok(())
}

/// The node's record after a change, with a warning when the coordinator
/// can't be read: failing now would guard nothing and hide the result, so
/// an unreadable coordinator only drops the effective status, and says so.
pub(crate) fn node_value(repo: &Repo, id: &str) -> Result<(Value, Vec<Diagnostic>), GraphError> {
    let config = config::load(&repo.root)?;
    let mut index = index::build(&repo.root, &config)?;
    let (view, warnings) = effective_or_warn(&repo.root, &mut index.nodes);
    let node = match resolve_qid(&index, id, None) {
        Resolution::Ok(qid) => index.get(&qid).cloned(),
        other => return Err(resolve_error("id", id, &other, None)),
    }
    .ok_or_else(crate::coord::no_node_matches)?;
    let value = records(view.as_ref(), &[&node], &[])
        .pop()
        .unwrap_or(Value::Null);
    Ok((value, warnings))
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct NewData {
    #[schemars(with = "NodeRecord")]
    pub node: Value,
}

pub fn new_node(root: &Path, actor: Option<String>, spec: NewSpec, json: bool) -> u8 {
    let report = (|| -> Result<Report<NewData>, GraphError> {
        crate::graph_repair::require_writable(root)?;
        let repo = Repo::open(root)?;
        let actor = repo.actor(actor);
        let mut store = repo.store()?;
        let now = now_ms();
        repo.observe(&mut store, now)?;
        let (id, _) = create(&repo, &mut store, &actor, &spec, now)?;
        let (node, warnings) = node_value(&repo, &id)?;
        Ok(Report::ok(NewData { node }).with(warnings))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("new", json, report, |d| {
        // The path is the repository's own text (SEC2): escaped.
        println!(
            "{}",
            output::shown_path(&format!(
                "created {} ({})",
                d.node["id"].as_str().unwrap_or("-"),
                d.node["path"].as_str().unwrap_or("-")
            ))
        );
    })
}

/// One node to edit: its leaf and the frontmatter fields to set, or (for
/// `retype`) its whole new content.
pub(crate) struct Edit {
    pub(crate) node: IndexNode,
    pub(crate) fields: Vec<(&'static str, FmValue)>,
    pub(crate) content: Option<String>,
}

/// The outcome of edits recorded under leases.
pub(crate) struct Recorded {
    pub(crate) commit: String,
    pub(crate) changesets: Vec<String>,
    /// Profile C: the `node:` leases the edits were written under, still
    /// held, so the caller records what it must (a `done` mark) before
    /// [`Recorded::release_store_leases`] ends them.
    pub(crate) store_leases: Vec<String>,
    /// The leases this call took itself, not ones the actor already held
    /// (`format` gives back only those it took).
    pub(crate) taken: Vec<String>,
}

impl Recorded {
    /// Ends the `work:` leases, once the caller has recorded everything.
    /// A failure leaves the edit and its records in place and the lease
    /// with its holder, who can release it or let it lapse.
    pub(crate) fn release_store_leases(
        &self,
        store: &mut Store,
        actor: &str,
        now: u64,
    ) -> Result<(), GraphError> {
        for lease in &self.store_leases {
            store
                .execute(&Command::Release {
                    owner: actor.to_string(),
                    lease: mdkg_coord::LeaseId(lease.clone()),
                    now,
                })
                .map_err(|e| {
                    store_error(e).with_hint(format!(
                        "the edit is written and recorded; your lease {lease} stays held: \
                         `{BIN} release {lease}`, or let it lapse"
                    ))
                })?;
        }
        Ok(())
    }
}

/// Takes (or reuses) a lease for each edited node, writes the fields, makes
/// one git commit of the leased scopes plus `created`, and records a
/// changeset per lease. Leases this call took are released if any is held
/// by someone else.
pub(crate) fn record_edits(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    edits: &[Edit],
    created: &[String],
    message: &str,
    now: u64,
) -> Result<Recorded, GraphError> {
    record_edits_checked(
        repo,
        store,
        actor,
        edits,
        created,
        message,
        now,
        &|_| Ok(()),
        None,
    )
}

/// [`record_edits`] for one edit whose changeset `mark` records something
/// about (`done`'s done mark): the mark goes in the same coordinator
/// transaction as the changeset, so nothing can land between them, and a
/// refused mark leaves neither recorded.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_edits_marked(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    edit: Edit,
    created: &[String],
    message: &str,
    now: u64,
    mark: &dyn Fn(mdkg_coord::ChangesetId) -> Command,
) -> Result<Recorded, GraphError> {
    record_edits_checked(
        repo,
        store,
        actor,
        &[edit],
        created,
        message,
        now,
        &|_| Ok(()),
        Some(mark),
    )
}

/// [`record_edits`], running `check` once the leases are held and before
/// anything is written (`retype` checks there that nothing it read has
/// moved on).
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_edits_checked(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    edits: &[Edit],
    created: &[String],
    message: &str,
    now: u64,
    check: &dyn Fn(&mut Store) -> Result<(), GraphError>,
    mark: Option<&dyn Fn(mdkg_coord::ChangesetId) -> Command>,
) -> Result<Recorded, GraphError> {
    crate::graph_repair::require_writable(&repo.root)?;
    let mut taken: Vec<String> = Vec::new();
    // A failure before the git commit puts the edited files back (from
    // snapshots taken once the leases are held, so nobody else's edit to a
    // shared file is overwritten), removes created ones, and only then
    // releases the leases this call took; after the commit, the work is in
    // git and stays.
    let undo = |store: &mut Store,
                taken: &[String],
                snapshots: &[(std::path::PathBuf, Option<Vec<u8>>)],
                remove_created: bool| {
        for (path, bytes) in snapshots {
            match bytes {
                Some(b) => {
                    atomic_write(path, b).ok();
                }
                None => {
                    std::fs::remove_file(path).ok();
                }
            }
        }
        if remove_created {
            // Never a published profile C node (a checkpoint `done` made):
            // another worktree may already depend on it, so it stays.
            for rel in created.iter().filter(|r| repo.store_scope(r).is_none()) {
                std::fs::remove_file(repo.root.join(rel)).ok();
            }
        }
        for id in taken {
            store
                .execute(&Command::Release {
                    owner: actor.to_string(),
                    lease: mdkg_coord::LeaseId(id.clone()),
                    now,
                })
                .ok();
        }
    };
    // Nothing is leased, written or put back through a linked folder out
    // of the repository (RC1 audit SEC3); a file created for this edit (a
    // checkpoint) goes too.
    for e in edits {
        if let Err(err) = crate::anchor::refuse_linked_folders(&repo.root, &e.node.path) {
            undo(store, &taken, &[], true);
            return Err(err);
        }
    }
    let leases = match take_leases(repo, store, actor, edits, now, &mut taken) {
        Ok(l) => l,
        Err(e) => {
            // Nothing was written yet, so only the created files go.
            undo(store, &taken, &[], true);
            return Err(e);
        }
    };
    if let Err(e) = check(store).and_then(|()| fault("leased")) {
        undo(store, &taken, &[], true);
        return Err(e);
    }
    // Each edited file as it is now, under the leases: a failure before
    // the git commit puts back exactly this, uncommitted edits included.
    let snapshots: Vec<(std::path::PathBuf, Option<Vec<u8>>)> = edits
        .iter()
        .map(|e| {
            let path = repo.root.join(&e.node.path);
            let bytes = std::fs::read(&path).ok();
            (path, bytes)
        })
        .collect();
    if let Err(e) = write_fields(repo, edits).and_then(|()| fault("written")) {
        undo(store, &taken, &snapshots, true);
        return Err(e);
    }
    // Profile C: a node in the store's work folder is written in
    // place under its `work:` lease and never committed; its lease ends
    // here, as a commit ends a git one. Only the rest reaches git.
    let is_store = coord::is_store_scope;
    let (store_leases, git_leases): (Vec<_>, Vec<_>) =
        leases.into_iter().partition(|(_, s)| is_store(s));
    let git_created: Vec<String> = created
        .iter()
        .filter(|p| repo.store_scope(p).is_none())
        .cloned()
        .collect();
    let mut recorded = if git_leases.is_empty() && git_created.is_empty() {
        Recorded {
            commit: String::new(),
            changesets: Vec::new(),
            store_leases: Vec::new(),
            taken: Vec::new(),
        }
    } else {
        match commit_and_record(
            repo,
            store,
            actor,
            &git_leases,
            &git_created,
            message,
            now,
            mark,
        ) {
            Ok(r) => r,
            Err((e, committed)) => {
                if committed {
                    undo(store, &taken, &[], false);
                } else {
                    undo(store, &taken, &snapshots, true);
                }
                return Err(e);
            }
        }
    };
    recorded.store_leases = store_leases.into_iter().map(|(id, _)| id).collect();
    recorded.taken = taken;
    Ok(recorded)
}

/// Takes (or reuses) a lease for each edited node. Leases taken are pushed
/// to `taken` as they're granted, so a caller can release them on failure.
fn take_leases(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    edits: &[Edit],
    now: u64,
    taken: &mut Vec<String>,
) -> Result<Vec<(String, Scope)>, GraphError> {
    // An owner name reaches other actors: the same rule as a claim's.
    crate::msg::check_actor_mailbox(actor, true)?;
    let mut leases: Vec<(String, Scope)> = Vec::new();
    for edit in edits {
        let leaf = repo.leaf(&edit.node.path)?;
        coord::check_scope_text(&leaf)?;
        let state = store.coordinator().map_err(store_error)?;
        // The node held under another path (a file renamed in some
        // worktree) is held all the same: never edited around its holder.
        let key = repo.node_key(&leaf, &edit.node.qid);
        if let Some(h) = state
            .node_holder(&key)
            .filter(|h| h.scope != leaf && h.deadline > now)
        {
            return Err(held_elsewhere(&edit.node.qid, h, actor));
        }
        // The same rule as a claim: a role or a turn never covers a node.
        let existing = coord::covering_lease(&state, actor, &leaf)
            .filter(|l| l.deadline > now)
            .map(|l| (l.id.to_string(), l.scope.clone()));
        if let Some(found) = existing {
            // Never under a lease an older build granted on a scope this
            // one refuses: it lapses instead.
            if !repo.lease_still_granted(&found.1) {
                return Err(GraphError::conflict(
                    Code::CoordLeaseHeld,
                    format!(
                        "your lease {} covers this, on a scope this build doesn't lease",
                        found.0
                    ),
                )
                .with_hint(format!(
                    "release it (`{BIN} release {}`) or let it lapse, then try again",
                    found.0
                )));
            }
            // Main moved past your lease only by your own commits: it's
            // settled as landed, together with a fresh lease on the same
            // scope, which this edit then uses.
            let node = (found.1 == leaf).then(|| key.clone());
            let lease = settle_landed(repo, store, actor, &found.0, &found.1, node, now)?;
            let found = match lease {
                Some(fresh) => {
                    taken.push(fresh.clone());
                    (fresh, found.1)
                }
                None => found,
            };
            // One folder lease covering several edits is committed once.
            if !leases.iter().any(|(id, _)| *id == found.0) {
                leases.push(found);
            }
            continue;
        }
        // The cap, read inside the claim's own transaction.
        let mut capped = false;
        let outcome = store.execute_with(|c| {
            if coord::at_lease_cap(c, actor, now) {
                capped = true;
                // Rolls the transaction back; `capped` says why.
                return Err(StoreError::Schema("capped".into()));
            }
            Ok(Command::Claim {
                owner: actor.to_string(),
                scope: leaf.clone(),
                mode: mdkg_coord::Mode::Exclusive,
                now,
                request: None,
                node: Some(key.clone()),
            })
        });
        if capped {
            return Err(coord::lease_cap_error());
        }
        let outcome = outcome.map_err(store_error)?;
        match outcome {
            Outcome::Lease { lease } => {
                taken.push(lease.to_string());
                leases.push((lease.to_string(), leaf));
            }
            Outcome::Refused { error } => {
                let state = store.coordinator().map_err(store_error)?;
                let holder = state
                    .holders(&leaf)
                    .next()
                    .or_else(|| state.node_holder(&key))
                    .cloned();
                return Err(match holder {
                    Some(h) => held_elsewhere(&edit.node.qid, &h, actor),
                    None => refusal(&error),
                });
            }
            other => {
                return Err(GraphError::failure(
                    Code::CoordStoreFailed,
                    format!("unexpected outcome {other:?}"),
                ));
            }
        }
    }
    Ok(leases)
}

/// Settles your lease `id` on `scope` when main moved past its base only by
/// work this worktree already has: you committed on main itself, or your
/// branch merged, without losing any outside work. That holds when
/// (i) main's content under the scope is this worktree's HEAD's
/// and (ii) no commit on main that changed the scope is missing from HEAD:
/// then main holds exactly your version, and you have seen every change
/// main made there. The lease is committed `landed` with the scope's
/// content as main has it, from main's tip (so it settles at once), and a
/// fresh lease on the same scope (based on main as it is) is taken in the
/// same transaction, so nobody can claim in between. Returns the fresh
/// lease's id; `None` leaves the lease as it was, and a real change on
/// main under it still ends it in conflict at commit.
fn settle_landed(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    id: &str,
    scope: &Scope,
    node: Option<String>,
    now: u64,
) -> Result<Option<String>, GraphError> {
    if coord::is_store_scope(scope) || coord::is_role_or_turn(scope) {
        return Ok(None);
    }
    let state = store.coordinator().map_err(store_error)?;
    let lease_id = mdkg_coord::LeaseId(id.to_string());
    let Some(lease) = state.lease(&lease_id) else {
        return Ok(None);
    };
    if state.snapshot(scope) == lease.base {
        return Ok(None);
    }
    let mode = lease.mode;
    // Main and HEAD pinned to commits once: every check, the content
    // named and the source are of these, so a main that moves meanwhile
    // can't slip an unseen change in.
    let branch = coord::main_ref(&repo.top);
    let main = git::commit_of(&repo.top, &branch).map_err(git_error)?;
    let head = git::commit_of(&repo.top, "HEAD").map_err(git_error)?;
    let path = scope.as_str();
    if !git::same_under(&repo.top, &main, &head, path).map_err(git_error)?
        || git::touched_between(&repo.top, &head, &main, path).map_err(git_error)?
    {
        return Ok(None);
    }
    // The scope's whole content as main has it: every file there, and
    // every leaf the coordinator holds there that main doesn't.
    let mut changes = Changes::new();
    for (leaf, content) in git::leaves(&repo.top, &main, path).map_err(git_error)? {
        if scope.contains(&leaf) {
            changes.insert(leaf, Some(content));
        }
    }
    for leaf in state.leaves().into_keys().filter(|l| scope.contains(l)) {
        changes.entry(leaf).or_insert(None);
    }
    if changes.is_empty() {
        return Ok(None);
    }
    let source = mdkg_coord::Source {
        commit: main,
        branch,
    };
    let commit = Command::Commit {
        owner: actor.to_string(),
        lease: lease_id,
        changes,
        source: Some(source),
        now,
        landed: true,
    };
    let chained = store
        .execute_then(&commit, |_, outcome| {
            matches!(outcome, Outcome::Committed { .. }).then(|| Command::Claim {
                owner: actor.to_string(),
                scope: scope.clone(),
                mode,
                now,
                request: None,
                node: node.clone(),
            })
        })
        .map_err(store_error)?;
    // Refused: main moved under the lease after all (its content isn't
    // what was checked), which ends the lease in conflict, before anything
    // is written.
    if let Some(error) = chained.first.error() {
        return Err(refusal(error).with_hint(format!(
            "main changed {} under your lease: merge main into your branch (`git merge {}`), \
             resolve it, and commit",
            output::shown(scope.as_str()),
            output::shown(&coord::main_ref(&repo.top))
        )));
    }
    Ok(match (chained.recorded, chained.second) {
        (true, Some(Outcome::Lease { lease })) => Some(lease.to_string()),
        _ => None,
    })
}

/// Another actor's lease on a node: its holder and scope as data, never in
/// the message, where a name another actor chose would read as ours.
fn held_elsewhere(qid: &str, h: &mdkg_coord::Lease, actor: &str) -> GraphError {
    // Your own lease under the node's old path (you moved the file, or a
    // move was merged): say so, and how to move the lease too.
    if h.owner == actor {
        return GraphError::new(
            mdkg_graph::ErrorKind::Conflict,
            mdkg_format::Diagnostic::error(
                Code::CoordLeaseHeld,
                format!(
                    "you hold {} under another path ({})",
                    output::display_ref(qid),
                    h.id
                ),
            )
            .at(h.scope.to_string(), None)
            .for_node(qid.to_string())
            .with_hint(format!(
                "release that lease (`{BIN} release {}`), then claim the node again",
                h.id
            )),
        );
    }
    GraphError::new(
        mdkg_graph::ErrorKind::Conflict,
        mdkg_format::Diagnostic::error(
            Code::CoordLeaseHeld,
            format!(
                "{} is held by another lease until {} ({})",
                output::display_ref(qid),
                rfc3339(h.deadline),
                h.id
            ),
        )
        .at(h.scope.to_string(), None)
        .held_by([h.owner.clone()])
        .for_node(qid.to_string())
        .with_hint("pick other work, or wait for the lease to end"),
    )
}

/// Writes each edit's frontmatter fields into its file.
fn write_fields(repo: &Repo, edits: &[Edit]) -> Result<(), GraphError> {
    let lex = lexical(&config::load(&repo.root)?);
    for edit in edits {
        let path = repo.root.join(&edit.node.path);
        let mut content = match &edit.content {
            Some(c) => c.clone(),
            None => std::fs::read_to_string(&path).map_err(|e| {
                GraphError::failure(
                    Code::IoReadFailed,
                    format!("can't read {}: {e}", edit.node.path),
                )
            })?,
        };
        for (key, value) in &edit.fields {
            let clamped;
            let value = match value {
                FmValue::String(v) if *key == "updated" => {
                    clamped =
                        FmValue::String(not_before(v, &[&edit.node.created, &edit.node.updated]));
                    &clamped
                }
                _ => value,
            };
            content = frontmatter::set_field_with(&content, &edit.node.path, key, value, lex)
                .map_err(|e| GraphError::from_format(&e, &repo.root))?;
        }
        atomic_write(&path, content.as_bytes()).map_err(|e| {
            GraphError::failure(
                Code::IoWriteFailed,
                format!("can't write {}: {e}", edit.node.path),
            )
        })?;
    }
    Ok(())
}

/// The test hook simulates crashes and failed writes: debug builds stop after
/// the step `MDKG_CRASH_AFTER` names, as a crash would, or fail it, as one
/// failed write would, when `MDKG_FAIL_AFTER` names it. Steps: `leased` and
/// `written` in [`record_edits`]; `marked` (the store's status) and
/// `released` (a store node's lease) in `done`; `marked` in `update`;
/// `staged` (the synced temporary file) and `linked` (the published name,
/// synced) in `new`.
/// Release builds never read either.
pub(crate) fn fault(step: &str) -> Result<(), GraphError> {
    if !cfg!(debug_assertions) {
        return Ok(());
    }
    if std::env::var("MDKG_CRASH_AFTER").ok().as_deref() == Some(step) {
        std::process::exit(86);
    }
    if std::env::var("MDKG_FAIL_AFTER").ok().as_deref() == Some(step) {
        return Err(GraphError::failure(
            Code::IoWriteFailed,
            format!("a write failed after `{step}` (injected)"),
        ));
    }
    Ok(())
}

/// Replaces a file whole: the bytes go to a temporary file beside it, are
/// synced, and are renamed over it, so a crash leaves the old file or the
/// new one, never a partial one; then the folder is synced, so the rename
/// itself survives a power loss.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(
        ".mdkg-write-{name}-{}.tmp",
        mdkg_format::Uuid7::now()
    ));
    // The file keeps its permissions: the temporary is private until it
    // has them.
    let existing = std::fs::metadata(path).ok().map(|m| m.permissions());
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if existing.is_some() {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut f = options.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Some(p) = &existing {
            std::fs::set_permissions(&tmp, p.clone())?;
        }
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        std::fs::remove_file(&tmp).ok();
    }
    result
}

/// Makes one git commit of the leased scopes plus `created`, and records a
/// changeset per lease. On failure, says whether the git commit was made.
#[allow(clippy::too_many_arguments)]
pub(crate) fn commit_and_record(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    leases: &[(String, Scope)],
    created: &[String],
    message: &str,
    now: u64,
    mark: Option<&dyn Fn(mdkg_coord::ChangesetId) -> Command>,
) -> Result<Recorded, (GraphError, bool)> {
    let before = |e: GraphError| (e, false);
    let after = |e: GraphError| (e, true);
    let created_leaves: Vec<String> = created
        .iter()
        .map(|p| repo.scope_for(p).to_string())
        .collect();
    let mut paths: Vec<String> = leases.iter().map(|(_, s)| s.to_string()).collect();
    paths.extend(created_leaves.iter().cloned());
    paths.sort();
    paths.dedup();
    // Every file recorded under a lease is shown to other actors (`status`,
    // sync notes): checked as a lease's scope is, before anything is
    // committed, so an unchecked path never enters the commit.
    for (_, scope) in leases {
        for f in git::files_under(&repo.top, scope.as_str())
            .map_err(git_error)
            .map_err(before)?
        {
            coord::check_scope_text(&Scope::new(f.as_str()))
                .map_err(|e| e.at(f.clone(), None))
                .map_err(before)?;
        }
    }
    let committed = git::commit_paths(&repo.top, &paths, message)
        .map_err(git_error)
        .map_err(before)?;
    let wrap = if committed { after } else { before };
    let source = git::head(&repo.top).map_err(git_error).map_err(wrap)?;
    let mut changesets = Vec::new();
    for (i, (lease, scope)) in leases.iter().enumerate() {
        let state = store.coordinator().map_err(store_error).map_err(wrap)?;
        let current = state.leaves();
        let mut changes = Changes::new();
        // One spelling per file: the disk's when the file is there (scopes
        // ignore case, and the coordinator may hold another spelling), the
        // coordinator's only for files that are gone.
        let mut files: BTreeMap<Scope, String> = BTreeMap::new();
        for f in git::files_under(&repo.top, scope.as_str())
            .map_err(git_error)
            .map_err(wrap)?
        {
            files.entry(Scope::new(f.as_str())).or_insert(f);
        }
        let mut extra: Vec<String> = current
            .keys()
            .filter(|l| scope.contains(l))
            .map(ToString::to_string)
            .collect();
        if i == 0 {
            extra.extend(created_leaves.iter().cloned());
        }
        for f in extra {
            files.entry(Scope::new(f.as_str())).or_insert(f);
        }
        for (leaf, file) in files {
            let have = git::file_digest(&repo.top, &file)
                .map_err(git_error)
                .map_err(wrap)?;
            if current.get(&leaf) != have.as_ref() {
                changes.insert(leaf, have);
            }
        }
        let commit = Command::Commit {
            owner: actor.to_string(),
            lease: mdkg_coord::LeaseId(lease.clone()),
            changes,
            source: Some(source.clone()),
            now,
            landed: false,
        };
        // The first lease's changeset carries the caller's mark, in the
        // same transaction.
        let chained = store
            .execute_then(&commit, |_, outcome| {
                match (mark.filter(|_| i == 0), outcome) {
                    (Some(mark), Outcome::Committed { changeset }) => Some(mark(*changeset)),
                    _ => None,
                }
            })
            .map_err(store_error)
            .map_err(wrap)?;
        if !chained.recorded {
            // The mark was refused, which rolled the changeset back too:
            // nothing is recorded and the lease is as it was, so running
            // the command again records both.
            let short = &source.commit[..source.commit.len().min(12)];
            let refused = chained
                .second
                .as_ref()
                .and_then(Outcome::error)
                .map(refusal)
                .unwrap_or_else(|| {
                    GraphError::failure(Code::CoordStoreFailed, "the record was refused")
                });
            let what = if created.is_empty() {
                "the work is"
            } else {
                "the work and its checkpoint are"
            };
            let message = format!(
                "{what} committed in {short}, but not recorded: {}",
                refused.diagnostic.message
            );
            return Err(wrap(
                GraphError::new(
                    refused.kind,
                    Diagnostic {
                        message,
                        hint: None,
                        ..*refused.diagnostic
                    },
                )
                .with_hint("run the same command again to record it"),
            ));
        }
        let outcome = chained.first;
        match outcome {
            Outcome::Committed { changeset } => changesets.push(changeset.to_string()),
            Outcome::Refused { error } if committed => {
                // The commit landed: say so first, then why it isn't
                // recorded. Nothing is to be run again: the work is in git,
                // and a second run would only find it done.
                let short = &source.commit[..source.commit.len().min(12)];
                let what = if created.is_empty() {
                    "the work is"
                } else {
                    "the work and its checkpoint are"
                };
                let refused = refusal(&error);
                let message = format!(
                    "{what} committed in {short}, but not recorded: {} ({})",
                    output::shown(scope.as_str()),
                    refused.diagnostic.message
                );
                let hint = format!(
                    "another change to it reached main while you held it: merge main into \
                     your branch (`git merge {}`), resolve {}, and commit; your work lands \
                     with your branch",
                    output::shown(&coord::main_ref(&repo.top)),
                    output::shown(scope.as_str())
                );
                return Err(wrap(
                    GraphError::new(
                        refused.kind,
                        Diagnostic {
                            message,
                            ..*refused.diagnostic
                        },
                    )
                    .with_hint(hint),
                ));
            }
            Outcome::Refused { error } => {
                return Err(wrap(refusal(&error)));
            }
            _ => {}
        }
    }
    Ok(Recorded {
        store_leases: Vec::new(),
        taken: Vec::new(),
        commit: source.commit,
        changesets,
    })
}

/// The node a typed handle names (a full id, a legacy id, a unique
/// prefix or a title fragment), with a note naming it when the handle
/// isn't its full id.
fn resolve_node(repo: &Repo, target: &str) -> Result<IndexNode, GraphError> {
    let config = config::load(&repo.root)?;
    let index = index::build(&repo.root, &config)?;
    noted_node(&index, target, "id").cloned()
}

/// What a title gave for a path-shaped `update --clear` target, held until
/// the recorded states are read: they come first.
enum Deferred {
    Node(Box<IndexNode>, Vec<Diagnostic>),
    Ambiguous(Vec<String>),
}

/// An edge flag of `new`, resolved from a handle: the target's qid,
/// and what the new file writes: a full id in a format 0.7 graph; on a
/// 0.6.0 graph the value as typed when it was the node's id or qid, else
/// its qid. An ambiguous handle refuses (exit 7); a missing one is
/// [`no_match`].
fn edge_value(
    index: &Index,
    v07: bool,
    key: &str,
    v: &str,
) -> Result<(String, String), GraphError> {
    let (resolved, notes) = resolve_noted(index, v, None);
    match resolved {
        Resolution::Ok(qid) => {
            let Some(n) = index.get(&qid) else {
                return Err(no_match(key));
            };
            notes.into_iter().for_each(output::notice);
            let written = if v07 {
                n.id.clone()
            } else if v.eq_ignore_ascii_case(&n.id) || v.eq_ignore_ascii_case(&n.qid) {
                v.to_lowercase()
            } else {
                n.qid.clone()
            };
            Ok((qid, written))
        }
        other @ Resolution::Ambiguous(_) => Err(handle_error(index, key, v, &other, None)),
        Resolution::Missing(_) => Err(no_match(key)),
    }
}

/// An edge flag's handle that matches no node: the one wording (the
/// orchestrator's E2/E4 ruling), with the flag named and the value not.
fn no_match(key: &str) -> GraphError {
    crate::coord::no_node_matches().with_hint(format!(
        "--{} names no node; `{bin} search <words>` or `{bin} list` finds it",
        key.replace('_', "-"),
        bin = crate::BIN
    ))
}

/// [`resolve_noted`]'s node, its notes added to the command's output.
pub(crate) fn noted_node<'a>(
    index: &'a Index,
    handle: &str,
    label: &str,
) -> Result<&'a IndexNode, GraphError> {
    let (resolved, notes) = resolve_noted(index, handle, None);
    match resolved {
        Resolution::Ok(qid) => {
            let node = index.get(&qid).ok_or_else(crate::coord::no_node_matches)?;
            notes.into_iter().for_each(output::notice);
            Ok(node)
        }
        other => Err(handle_error(index, label, handle, &other, None)),
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DoneData {
    #[schemars(with = "NodeRecord")]
    pub node: Value,
    /// The git commit holding the work.
    pub commit: String,
    pub changesets: Vec<String>,
    /// The checkpoint node created with `--checkpoint`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// The node was already done, with nothing of it left to finish:
    /// nothing was committed or recorded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub already_done: bool,
}

pub fn done(root: &Path, actor: Option<String>, target: &str, checkpoint: bool, json: bool) -> u8 {
    // Said whether or not what follows recording fails.
    let mut released = None;
    let report = (|| -> Result<Report<DoneData>, GraphError> {
        crate::graph_repair::require_writable(root)?;
        let repo = Repo::open(root)?;
        let node = resolve_node(&repo, target)?;
        if !WORK_TYPES.contains(&node.node_type.as_str()) || node.node_type == "goal" {
            let hint = if node.node_type == "goal" {
                format!("use `{BIN} goal done`")
            } else {
                "`done` is for work items".to_string()
            };
            // The handle was typed, so it isn't echoed in an error.
            return Err(bad(
                Code::NodeNotWork,
                format!("that node is a {}", node.node_type),
            )
            .with_hint(hint));
        }
        let actor = repo.actor(actor);
        // Refused before the store is touched.
        let leaf = repo.leaf(&node.path)?;
        // Already done, committed, and nothing of a `done` left to finish
        // (no lease of yours on it, no review or blocked state): says so,
        // and commits or records nothing, so a second `done` never makes a
        // second commit or changeset. An interrupted `done` (a crash or a
        // failed write after its file was written) goes on below and
        // finishes; a done mark it didn't get to is set here. A profile C
        // node's file is its record.
        if node.status.as_deref() == Some("done")
            && (coord::is_store_scope(&leaf)
                || git::unchanged_since_head(&repo.top, leaf.as_str()).map_err(git_error)?)
            && let Some(report) = already_done(&repo, &actor, &node, &leaf, checkpoint)?
        {
            return Ok(report);
        }
        let held = coord::require_authority_hold(&repo, &actor, &leaf, Some(&node.qid))?;
        let mut store = repo.store()?;
        let now = now_ms();
        repo.observe(&mut store, now)?;
        let mut created = Vec::new();
        let mut checkpoint_id = None;
        // Fail before writing a checkpoint if someone else holds the node,
        // here or under another name.
        let state = store.coordinator().map_err(store_error)?;
        let key = repo.node_key(&leaf, &node.qid);
        if let Some(h) = state
            .holders(&leaf)
            .find(|l| l.owner != actor && l.deadline > now)
            .or_else(|| {
                state
                    .node_holder(&key)
                    .filter(|h| h.scope != leaf && h.deadline > now)
            })
        {
            return Err(held_elsewhere(&node.qid, h, &actor));
        }
        // A checkpoint already records the work; finishing it must not
        // wrap it in another checkpoint, even with --checkpoint.
        if checkpoint && node.node_type != "checkpoint" {
            let spec = NewSpec {
                node_type: "checkpoint".into(),
                title: format!("{}: {}", output::display_ref(&node.qid), node.title),
                scope: vec![node.id.clone()],
                ..NewSpec::default()
            };
            let (id, rel) = create(&repo, &mut store, &actor, &spec, now)?;
            checkpoint_id = Some(id);
            created.push(rel);
        }
        let mut fields = vec![("status", FmValue::String("done".into()))];
        // Cleanup of an already-done local revision must not create a new
        // revision just by rewriting its generated completion timestamp.
        if !coord::is_store_scope(&leaf) || node.status.as_deref() != Some("done") {
            fields.push(("updated", FmValue::String(today(&repo, now))));
        }
        let edit = Edit {
            node: node.clone(),
            fields,
            content: None,
        };
        let message = format!("{}: done", output::display_ref(&node.qid));
        // On failure before its git commit, record_edits removes the
        // checkpoint again, so none is left for work that wasn't recorded.
        // A done mark, so other worktrees see it done before the merge: in
        // the same transaction as the changeset (and the end of the lease),
        // so no claim can land between them (RC1, O4).
        let mark_leaf = leaf.clone();
        let mark_actor = actor.clone();
        let mark = move |changeset| Command::SetWork {
            leaf: mark_leaf.clone(),
            status: Some("done".into()),
            changeset: Some(changeset),
            actor: mark_actor.clone(),
            now,
        };
        let recorded = record_edits_marked(
            &repo, &mut store, &actor, edit, &created, &message, now, &mark,
        )?;
        // Recorded: whatever fails next, the authority's lease goes too.
        released = coord::release_at_authority(&repo, &actor, &leaf, held.as_ref());
        // A local node has no changeset: its file is its lasting record.
        // Journal completion for receipts, then clear the temporary state
        // in the same transaction. Readers see no coordinator mark that
        // could outlast the file, and the lease is released only below.
        if recorded.changesets.is_empty() {
            let bytes = std::fs::read(repo.root.join(&node.path)).map_err(|e| {
                GraphError::failure(
                    Code::IoReadFailed,
                    format!("can't read completed node revision: {e}"),
                )
            })?;
            let revision = mdkg_format::Digest::of_bytes(&bytes).to_string();
            let marked = store
                .complete_local(
                    &leaf,
                    &repo.node_key(&leaf, &node.qid),
                    &actor,
                    &revision,
                    now,
                )
                .map_err(store_error)?;
            for outcome in std::iter::once(&marked.first).chain(marked.second.iter()) {
                if let Some(error) = outcome.error() {
                    return Err(refusal(error));
                }
            }
            if !marked.recorded || marked.second.is_none() {
                return Err(store_error(StoreError::Log(
                    "local completion and its status clear were not recorded together".into(),
                )));
            }
        }
        fault("marked")?;
        recorded.release_store_leases(&mut store, &actor, now)?;
        fault("released")?;
        let (value, warnings) = node_value(&repo, &node.qid)?;
        Ok(Report::ok(DoneData {
            node: value,
            commit: recorded.commit,
            changesets: recorded.changesets,
            checkpoint: checkpoint_id,
            already_done: false,
        })
        .with(warnings))
    })()
    .unwrap_or_else(Report::failed)
    .with(released);
    output::emit("done", json, report, |d| {
        // The id is node-derived: escaped like a path (RC1 SEC2).
        let id = d.node["id"].as_str().unwrap_or("-");
        let line = if d.already_done {
            format!("{id} was already done")
        } else if d.commit.is_empty() {
            // A profile C node: its file is its record, nothing reaches git.
            format!("done {id}")
        } else {
            format!(
                "done {id} in {} ({})",
                &d.commit[..d.commit.len().min(12)],
                d.changesets.join(", ")
            )
        };
        println!("{}", output::shown_path(&line));
    })
}

/// `done` on a node whose committed file already says done. `None` hands
/// it to the full command: your lease on it is still held (an interrupted
/// `done`, rerun; it may make one more commit, and the lease's own check
/// still guards it), or, for a profile C node (its file is its record), a
/// review or blocked state is left.
/// Otherwise nothing is committed:
/// - this version is recorded (a changeset of yours holding exactly HEAD's
///   bytes; its mark is set, and a review or blocked state cleared, if
///   missing) or main already holds it: the no-op, saying so;
/// - it isn't, and main changed the node in a commit HEAD lacks: the
///   conflict, with how to merge;
/// - it isn't (only after a crash): an error naming the commands that
///   record it.
fn already_done(
    repo: &Repo,
    actor: &str,
    node: &IndexNode,
    leaf: &Scope,
    checkpoint: bool,
) -> Result<Option<Report<DoneData>>, GraphError> {
    let mut store = repo.store()?;
    let now = now_ms();
    repo.observe(&mut store, now)?;
    let state = store.coordinator().map_err(store_error)?;
    let leased = state
        .holders(leaf)
        .any(|l| l.owner == actor && l.deadline > now);
    let pending = state.work(leaf).is_some();
    let skipped = if checkpoint {
        "; no checkpoint written"
    } else {
        ""
    };
    let said = |value: Value, warnings: Vec<Diagnostic>, commit: String| {
        Report::ok(DoneData {
            node: value,
            commit,
            changesets: Vec::new(),
            checkpoint: None,
            already_done: true,
        })
        .with(warnings)
        .with([Diagnostic::warning(
            Code::CoordNodeDone,
            format!(
                "{} is already done: nothing to commit or record{skipped}",
                output::display_ref(&node.qid)
            ),
        )])
    };
    if coord::is_store_scope(leaf) {
        if leased || pending {
            return Ok(None);
        }
        let (value, warnings) = node_value(repo, &node.qid)?;
        return Ok(Some(said(value, warnings, String::new())));
    }
    // HEAD and main pinned once; this version is HEAD's bytes.
    let branch = coord::main_ref(&repo.top);
    let head = git::commit_of(&repo.top, "HEAD").map_err(git_error)?;
    let version = git::leaves(&repo.top, &head, leaf.as_str())
        .map_err(git_error)?
        .get(leaf)
        .cloned();
    // Sets the done mark (with the changeset when there is one), which
    // also clears a review or blocked state.
    let mut mark = |changeset: Option<mdkg_coord::ChangesetId>| -> Result<(), GraphError> {
        let outcome = store
            .execute(&Command::SetWork {
                leaf: leaf.clone(),
                status: Some("done".into()),
                changeset,
                actor: actor.to_string(),
                now,
            })
            .map_err(store_error)?;
        match outcome.error() {
            Some(error) => Err(refusal(error)),
            None => Ok(()),
        }
    };
    // A changeset of yours holding exactly this version.
    let recorded = state
        .changesets()
        .filter(|c| {
            c.owner == actor
                && c.state != Settlement::Abandoned
                && c.changes.get(leaf).is_some_and(|v| *v == version)
        })
        .map(|c| (c.id, c.state))
        .max_by_key(|(id, _)| id.0);
    if let Some((changeset, settled)) = recorded {
        let marked = state.done_mark(leaf) == Some(changeset);
        if settled == Settlement::Committed {
            // Settled: a review or blocked state is cleared only while main
            // still holds this version, judged inside the write; one set on
            // a version main moved on to stays.
            if pending {
                let mut moved = false;
                let outcome = store.execute_with(|c| {
                    if c.leaf(leaf).and_then(|l| l.current()) != version.as_deref() {
                        moved = true;
                        // Rolls the transaction back; `moved` says why.
                        return Err(mdkg_coord_sqlite::StoreError::Schema("moved".into()));
                    }
                    Ok(Command::SetWork {
                        leaf: leaf.clone(),
                        status: Some("done".into()),
                        changeset: Some(changeset),
                        actor: actor.to_string(),
                        now,
                    })
                });
                match outcome {
                    Err(_) if moved => {}
                    Err(e) => return Err(store_error(e)),
                    Ok(outcome) => {
                        if let Some(error) = outcome.error() {
                            return Err(refusal(error));
                        }
                    }
                }
            }
        } else if pending || !marked {
            mark(Some(changeset))?;
        }
        let (value, warnings) = node_value(repo, &node.qid)?;
        return Ok(Some(said(value, warnings, head)));
    }
    let main = git::commit_of(&repo.top, &branch).map_err(git_error)?;
    // Main already has this version: it landed there.
    if git::same_under(&repo.top, &main, &head, leaf.as_str()).map_err(git_error)? {
        // A review or blocked state is cleared only while the coordinator
        // still holds this version, judged inside the write: one set on a
        // version main moved on to meanwhile stays.
        if pending {
            let mut moved = false;
            let outcome = store.execute_with(|c| {
                if c.leaf(leaf).and_then(|l| l.current()) != version.as_deref() {
                    moved = true;
                    // Rolls the transaction back; `moved` says why.
                    return Err(mdkg_coord_sqlite::StoreError::Schema("moved".into()));
                }
                Ok(Command::SetWork {
                    leaf: leaf.clone(),
                    status: Some("done".into()),
                    changeset: None,
                    actor: actor.to_string(),
                    now,
                })
            });
            match outcome {
                Err(_) if moved => {}
                Err(e) => return Err(store_error(e)),
                Ok(outcome) => {
                    if let Some(error) = outcome.error() {
                        return Err(refusal(error));
                    }
                }
            }
        }
        let (value, warnings) = node_value(repo, &node.qid)?;
        return Ok(Some(said(value, warnings, head)));
    }
    // Not recorded. Main changed the node in a commit HEAD lacks: a
    // conflict, never recorded over.
    if git::touched_between(&repo.top, &head, &main, leaf.as_str()).map_err(git_error)? {
        return Err(GraphError::conflict(
            Code::CoordCasFailed,
            format!(
                "{} is done here but not recorded: main changed it under your lease",
                output::display_ref(&node.qid)
            ),
        )
        .with_hint(format!(
            "merge main into your branch (`git merge {}`), resolve {}, and commit; your work \
             lands with your branch",
            output::shown(&branch),
            output::shown(leaf.as_str())
        )));
    }
    // Your lease is still held: the full command reruns, and the lease's
    // own check guards it (main's changes since were checked just above).
    if leased {
        return Ok(None);
    }
    // Committed but not recorded: a `done` stopped after its commit.
    let handle = output::display_ref(&node.qid);
    Err(GraphError::not_found(
        Code::CoordNoLease,
        format!("{handle} is done and committed here, but not recorded (a done was interrupted)"),
    )
    .with_hint(format!(
        "record it: `{BIN} claim {handle}`, then `{BIN} done {handle}` (which may make one more \
         commit)"
    )))
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UpdateData {
    /// The node's id; for `--clear` on a state whose node isn't in this
    /// worktree, the file path it was recorded at.
    pub node: String,
    /// The effective status after the update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

pub fn update(
    root: &Path,
    actor: Option<String>,
    target: &str,
    status: Option<&str>,
    clear: bool,
    json: bool,
) -> u8 {
    let report = (|| -> Result<Report<UpdateData>, GraphError> {
        crate::graph_repair::require_writable(root)?;
        let wanted = status.map(str::to_lowercase);
        let set = match (wanted.as_deref(), clear) {
            (_, true) => None,
            (Some(s), false) if TEMPORARY.contains(&s) => Some(s.to_string()),
            (Some("progress"), false) => {
                return Err(
                    bad(Code::UsageBadStatus, "progress comes from holding a lease")
                        .with_hint(format!("run `{BIN} claim <id>`")),
                );
            }
            // Don't echo typed text, which may contain secrets.
            (Some(_), false) => {
                return Err(bad(Code::UsageBadStatus, "that isn't a temporary status")
                    .with_hint("`update` sets review or blocked; finish work with `done`"));
            }
            (None, false) => {
                return Err(bad(
                    Code::UsageBadStatus,
                    "give --status review|blocked or --clear",
                ));
            }
        };
        let repo = Repo::open(root)?;
        // One index for the command (a typed path is resolved against the
        // graph once more, by `path_leaf`).
        let config = config::load(&repo.root)?;
        let index = index::build(&repo.root, &config)?;
        // What was typed isn't quoted. A handle names a node,
        // except that an existing path given to `--clear` stays a
        // path; one that's gone falls through to the recorded states.
        let path_like = clear && repo.root.join(target).exists();
        let (resolved, notes) = if path_like {
            (resolve_qid(&index, target, None), Vec::new())
        } else {
            resolve_noted(&index, target, None)
        };
        // A missing path given to `--clear` may be a recorded state's: a
        // title it also matches (not its exact id) waits until the stored
        // states are read, and names the node only if none is there.
        let deferrable = clear
            && target.contains('/')
            && !matches!(resolve_qid(&index, target, None), Resolution::Ok(_));
        let ambiguous = |candidates: &[String]| {
            GraphError::ambiguous(format!("that handle names {} nodes", candidates.len()))
                .with_hint(format!(
                    "matches: {}; give more of the id, or the full id",
                    describe(&index, candidates).join("; ")
                ))
        };
        let mut deferred: Option<Deferred> = None;
        let mut node = match resolved {
            Resolution::Ok(qid) if deferrable => {
                deferred = index
                    .get(&qid)
                    .cloned()
                    .map(|n| Deferred::Node(Box::new(n), notes));
                None
            }
            Resolution::Ambiguous(candidates) if deferrable => {
                deferred = Some(Deferred::Ambiguous(candidates));
                None
            }
            Resolution::Ok(qid) => {
                notes.into_iter().for_each(output::notice);
                index.get(&qid).cloned()
            }
            // The candidates come from the graph, so they may be named.
            Resolution::Ambiguous(candidates) => return Err(ambiguous(&candidates)),
            // A state whose node isn't in this worktree (deleted on main,
            // only on an abandoned branch, or on someone else's branch) is
            // cleared by its file path, even when the node is no longer here.
            _ if clear => None,
            _ => {
                return Err(crate::coord::no_node_matches());
            }
        };
        let named = coord::named_actor(actor.as_ref());
        let actor = repo.actor(actor);
        // A state's owner is a mailbox name shown to other actors: check it
        // before storing it, as with a claim's owner. Clearing stores nothing,
        // so an older build's state is still cleared by its owner.
        if set.is_some() {
            crate::msg::check_actor_mailbox(&actor, named)?;
        }
        // A node's leaf is guarded (no `role:`/`run:` folder) and checked
        // with a lease authority before the store opens; a path's is found
        // in the stored states below, then checked the same way.
        // A typed path is resolved now too, so a refused one never opens
        // the store.
        let from_root = match &node {
            Some(_) => None,
            None => match (path_scope(&repo, target), deferred.take()) {
                (Ok(scope), d) => {
                    deferred = d;
                    scope
                }
                // Not a path that may be cleared: a title deferred for it
                // names its node after all.
                (Err(_), Some(Deferred::Node(n, notes))) => {
                    notes.into_iter().for_each(output::notice);
                    node = Some(*n);
                    None
                }
                (Err(_), Some(Deferred::Ambiguous(c))) => return Err(ambiguous(&c)),
                (Err(e), None) => return Err(e),
            },
        };
        let node_leaf = match &node {
            Some(n) => Some(repo.leaf(&n.path)?),
            None => None,
        };
        if let (Some(leaf), Some(n)) = (&node_leaf, &node) {
            coord::require_authority_hold(&repo, &actor, leaf, Some(&n.qid))?;
        }
        let mut store = repo.store()?;
        let now = now_ms();
        repo.observe(&mut store, now)?;
        let (leaf, label) = match (&node, node_leaf) {
            (Some(n), Some(leaf)) => (leaf, output::display_ref(&n.qid)),
            _ => {
                let state = store.coordinator().map_err(store_error)?;
                match (
                    path_leaf(&repo, &index, &state, target, from_root),
                    deferred,
                ) {
                    (Ok(leaf), _) => {
                        // A path that names a node here is that node.
                        node = node_at(&repo, &index, &leaf);
                        coord::require_authority_hold(
                            &repo,
                            &actor,
                            &leaf,
                            node.as_ref().map(|n| n.qid.as_str()),
                        )?;
                        let label = match &node {
                            Some(n) => output::display_ref(&n.qid),
                            // The path as the coordinator stored it, not as
                            // typed.
                            None => state
                                .work_states()
                                .find(|(k, _)| **k == leaf)
                                .map_or(leaf.to_string(), |(k, _)| k.to_string()),
                        };
                        (leaf, label)
                    }
                    // No state at that path: the title names the node, or
                    // several.
                    (Err(e), Some(Deferred::Ambiguous(c))) if e.code() == Code::NodeNotFound => {
                        return Err(ambiguous(&c));
                    }
                    (Err(e), Some(Deferred::Node(n, notes))) if e.code() == Code::NodeNotFound => {
                        notes.into_iter().for_each(output::notice);
                        let leaf = repo.leaf(&n.path)?;
                        coord::require_authority_hold(&repo, &actor, &leaf, Some(&n.qid))?;
                        let label = output::display_ref(&n.qid);
                        node = Some(*n);
                        (leaf, label)
                    }
                    (Err(e), _) => return Err(e),
                }
            }
        };
        // A state set is shown to others; clearing one stores nothing new.
        if set.is_some() {
            coord::check_scope_text(&leaf)?;
        }
        // Only the actor that set a state may clear it by path, since its
        // node may be alive on another branch (orchestrator, 2026-10-21).
        // Checked on the locked state, so a state set meanwhile by someone
        // else isn't cleared.
        let by_path = node.is_none();
        let mut refused: Option<Option<String>> = None;
        let outcome = store.execute_with(|c| {
            match path_refusal(c, by_path, &leaf, &actor) {
                Some(owner) => {
                    refused = Some(owner);
                    // An error from here rolls the transaction back, so
                    // nothing is written. The error itself is a stand-in:
                    // `refused` says why, and is read before it below.
                    Err(StoreError::Schema("refused".into()))
                }
                None => Ok(Command::SetWork {
                    leaf: leaf.clone(),
                    status: set.clone(),
                    changeset: None,
                    actor: actor.clone(),
                    now,
                }),
            }
        });
        // Before `outcome`: a refusal mustn't surface as a store error.
        match refused {
            Some(None) => return Err(e_not_found()),
            Some(Some(owner)) => {
                // Who set it is data, never prose: they chose the name.
                return Err(GraphError::new(
                    ErrorKind::Conflict,
                    Diagnostic::error(
                        Code::CoordNotOwner,
                        "another actor set that state, and its node isn't in this worktree",
                    )
                    .at(leaf.to_string(), None)
                    .held_by([owner])
                    .with_hint(
                        "ask the actor in `holders`: the node may be alive on their branch; to \
                         clear it anyway, run as them with --as",
                    ),
                ));
            }
            None => {}
        }
        let outcome = outcome.map_err(store_error)?;
        if let Some(error) = outcome.error() {
            return Err(refusal(error));
        }
        fault("marked")?;
        // The effective status after the update, from the same index.
        let mut warnings = Vec::new();
        let status = node.and_then(|mut n| {
            warnings = effective_or_warn(&repo.root, std::slice::from_mut(&mut n)).1;
            n.status
        });
        Ok(Report::ok(UpdateData {
            node: label,
            status,
        })
        .with(warnings))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("update", json, report, |d| {
        println!(
            "{}",
            output::shown_path(&format!(
                "{} is {}",
                d.node,
                d.status.as_deref().unwrap_or("-")
            ))
        );
    })
}

/// Neither a node nor a path with a recorded state. What was typed isn't
/// quoted, because it may contain secrets.
fn e_not_found() -> GraphError {
    GraphError::not_found(
        Code::NodeNotFound,
        "no node, and no review or blocked state recorded at that path",
    )
    .with_hint(format!("`update --clear` takes a node id, or a path `{BIN} status` lists for a state whose node isn't in this worktree"))
}

/// Where a typed path resolves from the graph root (`None` when nothing is
/// there), refused without echoing typed text: outside the worktree reads as
/// not found, and a `role:`/`run:` folder as that overlap.
fn path_scope(repo: &Repo, target: &str) -> Result<Option<Scope>, GraphError> {
    let overlap = || {
        GraphError::usage(Code::MsgBadId, "that path would overlap a role or a turn")
            .with_hint("`role:<name>` and `run:<name>` name roles and turns, never paths")
    };
    // A profile C node's state, by the work: name `status` lists it under.
    if coord::is_store_name(target) && !target.contains("..") && !target.contains('\\') {
        return Ok(Some(Scope::new(target)));
    }
    match repo.resolve(target) {
        // A role or a turn itself (`run:x`) has no state to clear.
        Ok(t) if coord::is_role_or_turn(&t.scope) => Err(overlap()),
        Ok(t) => Ok(Some(t.scope)),
        Err(e) if e.code() == Code::NodeNotFound => Ok(None),
        Err(e) if e.code() == Code::UsagePathOutsideWorktree => Err(e_not_found()),
        Err(e) if e.code() == Code::MsgBadId => Err(overlap()),
        Err(e) => Err(e),
    }
}

/// The leaf a `--clear` path names: relative to the graph root, as `claim`
/// reads paths (`from_root`, from `path_scope`), or to the worktree's top
/// folder, as `status` prints them (they differ when the graph sits in a
/// subfolder).
fn path_leaf(
    repo: &Repo,
    index: &Index,
    state: &mdkg_coord::Coordinator,
    target: &str,
    from_root: Option<Scope>,
) -> Result<Scope, GraphError> {
    // The stored spelling can still name a scope under a role's or a turn's
    // folder (a state an older build wrote for a graph inside one): never
    // that, since the lease authority isn't asked about such scopes.
    [from_root, Some(Scope::new(target))]
        .into_iter()
        .flatten()
        .filter(|leaf| coord::is_store_scope(leaf) || !coord::aliases_named(leaf.as_str()))
        .find(|leaf| state.work(leaf).is_some() || node_at(repo, index, leaf).is_some())
        .ok_or_else(e_not_found)
}

/// Why a path clear is refused, judged on the locked state: `Some(owner)`
/// when someone else (`Some(Some(actor))`) set the state, or nobody does any
/// more (`Some(None)`); `None` when it may go ahead.
fn path_refusal(
    state: &mdkg_coord::Coordinator,
    by_path: bool,
    leaf: &Scope,
    actor: &str,
) -> Option<Option<String>> {
    if !by_path {
        return None;
    }
    let owner = state.work(leaf).map(|w| w.actor.clone());
    (owner.as_deref() != Some(actor)).then_some(owner)
}

/// The node in this worktree whose file is `leaf`, if any.
fn node_at(repo: &Repo, index: &Index, leaf: &Scope) -> Option<IndexNode> {
    index
        .nodes
        .iter()
        .find(|n| repo.scope_for(&n.path) == *leaf)
        .cloned()
}

/// Whether `next` may offer a node: a work item with a ready status (in the
/// preference list, and not `review` or `blocked`: another agent's state in
/// the coordinator, or a status a 0.6.0-written file still carries), with
/// nothing unfinished blocking it (the ready set), not leased
/// by someone else.
fn available(
    index: &Index,
    n: &IndexNode,
    config: &Config,
    view: Option<&View>,
    actor: &str,
    prefix: &str,
) -> bool {
    let status = n.status.as_deref();
    // A node in a folder named like a role or a turn, or in a workspace
    // aliased `run` or `role`, can't be claimed by id: never offer it.
    !coord::aliases_named(&format!("{prefix}{}", n.path))
        && !matches!(n.ws.as_str(), "run" | "role")
        && NEXT_TYPES.contains(&n.node_type.as_str())
        && status.is_some_and(|s| {
            !TEMPORARY.contains(&s) && config.next_status_preference.iter().any(|p| p == s)
        })
        && mdkg_index::graph::open_blockers(index, n).is_empty()
        && view
            .and_then(|v| v.holder(&n.path, Some(&n.qid)))
            .is_none_or(|l| l.owner == actor)
}

/// Picks the next work item: available nodes (see [`available`]), by
/// priority, then status preference, then id.
fn pick<'a>(
    index: &Index,
    candidates: impl Iterator<Item = &'a IndexNode>,
    config: &Config,
    view: Option<&View>,
    actor: &str,
    prefix: &str,
) -> Option<&'a IndexNode> {
    let rank =
        |s: Option<&str>| s.and_then(|s| config.next_status_preference.iter().position(|p| p == s));
    let mut ready: Vec<&IndexNode> = candidates
        .filter(|n| available(index, n, config, view, actor, prefix))
        .collect();
    ready.sort_by(|a, b| {
        let p = |n: &IndexNode| n.priority.unwrap_or(config.priority_max + 1);
        p(a).cmp(&p(b))
            .then(rank(a.status.as_deref()).cmp(&rank(b.status.as_deref())))
            .then(a.qid.cmp(&b.qid))
    });
    ready.first().copied()
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct NextData {
    /// The full record, body included; absent when nothing is ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<NodeRecord>")]
    pub node: Option<Value>,
}

fn none_ready() -> Diagnostic {
    Diagnostic::new(Code::NodeNoneReady, Severity::Info, "no work item is ready")
        .with_hint(format!("create one with `{BIN} new task \"...\"`"))
}

pub fn next(root: &Path, actor: Option<String>, id: Option<&str>, json: bool) -> u8 {
    let report = (|| -> Result<Report<NextData>, GraphError> {
        let config = config::load(root)?;
        let mut index = index::build(root, &config)?;
        // `next` hands out work: never as if nobody held anything.
        let view = effective_or_fail(root, &mut index.nodes)?;
        coord::authority_usable(root)?;
        let opened = Repo::open(root);
        let prefix = opened
            .as_ref()
            .map(|r| r.prefix.clone())
            .unwrap_or_default();
        let actor = opened
            .map(|r| r.actor(actor.clone()))
            .unwrap_or_else(|_| actor.clone().unwrap_or_else(|| "agent".into()));
        if let Some(id) = id {
            let qid = noted_node(&index, id, "id")?.qid.clone();
            // Follow the `next` chain to the first item that's available.
            let mut seen = std::collections::BTreeSet::new();
            let mut at = index.get(&qid).and_then(|n| n.edges.next.clone());
            while let Some(q) = at.filter(|q| seen.insert(q.clone())) {
                let Some(n) = index.get(&q) else { break };
                if available(&index, n, &config, view.as_ref(), &actor, &prefix) {
                    let node = records(view.as_ref(), &[n], &[]).pop();
                    return Ok(Report::ok(NextData { node }));
                }
                at = n.edges.next.clone();
            }
        }
        let picked = pick(
            &index,
            index.nodes.iter(),
            &config,
            view.as_ref(),
            &actor,
            &prefix,
        );
        let node = picked.and_then(|n| records(view.as_ref(), &[n], &[]).pop());
        let empty = node.is_none();
        let report = Report::ok(NextData { node });
        Ok(if empty {
            report.with([none_ready()])
        } else {
            report
        })
    })()
    .unwrap_or_else(Report::failed);
    // It picks by local holders: a lease held through an authority isn't
    // seen here.
    let notice = report
        .failure
        .is_none()
        .then(|| coord::authority_notice(root))
        .flatten();
    let report = report.with(notice);
    output::emit("next", json, report, |d| {
        if let Some(n) = &d.node {
            println!("{}", crate::read::show_text(n));
        }
    })
}

/// What a `goal` subcommand does.
#[derive(Debug, Clone)]
pub enum GoalAction {
    Current,
    Next(Option<String>),
    Activate(String),
    Pause(String),
    Done(String),
    /// 0.6.0's `goal claim`: claim the goal's next item.
    Claim(Option<String>),
}

impl GoalAction {
    /// The output's schema name: one per subcommand (the orchestrator's D3).
    pub fn name(&self) -> &'static str {
        match self {
            GoalAction::Current => "goal.current",
            GoalAction::Next(_) => "goal.next",
            GoalAction::Activate(_) => "goal.activate",
            GoalAction::Pause(_) => "goal.pause",
            GoalAction::Done(_) => "goal.done",
            GoalAction::Claim(_) => "goal.claim",
        }
    }
}

/// `goal current`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct GoalCurrentData {
    /// The active goal; absent when none is.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<NodeRecord>")]
    pub goal: Option<Value>,
    /// Active leases on items in the goal's scope.
    pub leases: Vec<LeaseRecord>,
}

/// `goal next`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct GoalNextData {
    #[schemars(with = "NodeRecord")]
    pub goal: Value,
    /// The item to work on; absent when none is ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<NodeRecord>")]
    pub node: Option<Value>,
}

/// `goal activate`, `goal pause` and `goal done`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct GoalChangeData {
    /// The goal as it is now.
    #[schemars(with = "NodeRecord")]
    pub goal: Value,
    /// The changesets recorded.
    pub changesets: Vec<String>,
}

/// 0.6.0's `goal claim`: `goal next`, and the lease it took.
#[derive(Debug, Serialize, JsonSchema)]
pub struct GoalClaimData {
    #[schemars(with = "NodeRecord")]
    pub goal: Value,
    /// The item claimed; absent when none is ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<NodeRecord>")]
    pub node: Option<Value>,
    /// The lease it took.
    pub leases: Vec<LeaseRecord>,
}

/// A report's data as another type, diagnostics and failure kept.
fn map_report<A, B>(report: Report<A>, f: impl FnOnce(A) -> B) -> Report<B> {
    Report {
        data: report.data.map(f),
        diagnostics: report.diagnostics,
        failure: report.failure,
    }
}

/// What `goal` prints in text mode, whichever subcommand.
fn goal_text(goal: Option<&Value>, leases: &[LeaseRecord], node: Option<&Value>) {
    if let Some(g) = goal {
        // Its title too (RC1 F6), escaped like every node's text (SEC2).
        println!(
            "{}",
            output::shown_path(&format!(
                "goal {} ({}): {}",
                g["id"].as_str().unwrap_or("-"),
                g["fields"]["goal_state"].as_str().unwrap_or("-"),
                g["title"].as_str().unwrap_or("-")
            ))
        );
    }
    for l in leases {
        // Another actor's lease, maybe from an older store: escaped.
        println!(
            "{}",
            output::shown(&format!(
                "lease {} on {} by {} until {}",
                l.id, l.scope, l.owner, l.until
            ))
        );
    }
    if let Some(n) = node {
        println!("\nnext:\n{}", crate::read::show_text(n));
    }
}

/// What `goal`'s subcommands find, before each is given its own shape.
#[derive(Debug, Default)]
pub struct GoalData {
    pub goal: Option<Value>,
    /// For `goal next` and `goal claim`: the item to work on.
    pub node: Option<Value>,
    /// Active leases on items in the goal's scope; for `goal claim`, the
    /// lease it took.
    pub leases: Vec<LeaseRecord>,
    /// For edits: the changesets recorded.
    pub changesets: Vec<String>,
}

fn goal_state(n: &IndexNode) -> Option<&str> {
    n.attributes.get("goal_state").and_then(|v| v.as_str())
}

fn active_goals(index: &Index) -> Vec<&IndexNode> {
    let mut goals: Vec<&IndexNode> = index
        .nodes
        .iter()
        .filter(|n| n.node_type == "goal" && goal_state(n) == Some("active"))
        .collect();
    goals.sort_by(|a, b| a.qid.cmp(&b.qid));
    goals
}

pub fn goal(root: &Path, actor: Option<String>, action: GoalAction, json: bool) -> u8 {
    let name = action.name();
    // Said whether or not what follows recording fails.
    let mut released = Vec::new();
    let report = (|| -> Result<Report<GoalData>, GraphError> {
        // `goal current` only reports, so it works where git refuses the
        // repository (with a warning below); the others need the repository.
        let opened = match Repo::open(root) {
            Ok(r) => Some(r),
            Err(_) if matches!(action, GoalAction::Current) => None,
            Err(e) => return Err(e),
        };
        let repo = || {
            opened.as_ref().ok_or_else(|| {
                GraphError::failure(Code::IoGitFailed, "coordination needs a git repository")
            })
        };
        let actor = opened
            .as_ref()
            .map(|r| r.actor(actor.clone()))
            .unwrap_or_default();
        let config = config::load(root)?;
        let mut index = index::build(root, &config)?;
        // `goal next` and `goal claim` hand out work, so they need the
        // leases; `goal current` only reports, so it warns and goes on.
        let (view, skipped) = if matches!(action, GoalAction::Current) {
            effective_or_warn(root, &mut index.nodes)
        } else {
            (effective_or_fail(root, &mut index.nodes)?, Vec::new())
        };
        let pick_goal = |id: Option<&str>| -> Result<IndexNode, GraphError> {
            match id {
                Some(id) => {
                    let node = noted_node(&index, id, "goal")?.clone();
                    // The handle was typed, so it isn't echoed in an error.
                    if node.node_type != "goal" {
                        return Err(bad(
                            Code::GoalNotGoal,
                            format!("that node is a {}, not a goal", node.node_type),
                        )
                        .with_hint(format!("`{} list --type goal` lists the goals", crate::BIN)));
                    }
                    Ok(node)
                }
                None => active_goals(&index)
                    .first()
                    .map(|g| (*g).clone())
                    .ok_or_else(|| {
                        GraphError::not_found(Code::GoalNoneActive, "no goal is active")
                            .with_hint(format!("activate one with `{BIN} goal activate <goal-id>`"))
                    }),
            }
        };
        let goal_record = |g: &IndexNode| records(view.as_ref(), &[g], &[]).pop();
        match action {
            GoalAction::Current => {
                let goals = active_goals(&index);
                let Some(goal) = goals.first() else {
                    return Ok(Report::ok(GoalData::default()).with(skipped.clone()).with([
                        Diagnostic::new(Code::GoalNoneActive, Severity::Info, "no goal is active"),
                    ]));
                };
                let scope = goal_scope(&index, goal);
                let leases = view
                    .as_ref()
                    .map(|v| {
                        scope
                            .qids
                            .iter()
                            .filter_map(|q| index.get(q))
                            .filter_map(|n| v.holder(&n.path, Some(&n.qid)))
                            .map(lease_record)
                            .collect()
                    })
                    .unwrap_or_default();
                let mut report = Report::ok(GoalData {
                    goal: goal_record(goal),
                    leases,
                    ..GoalData::default()
                })
                .with(skipped.clone())
                // Its active node may be in another clone's store (E1).
                .with(mdkg_graph::graph_checks::absent_pointers(&index, goal));
                if goals.len() > 1 {
                    report = report.with([Diagnostic::warning(
                        Code::GoalMultipleActive,
                        format!(
                            "{} goals are active: {}",
                            goals.len(),
                            goals
                                .iter()
                                .map(|g| output::display_ref(&g.qid))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                    .with_hint(format!(
                        "activate exactly one with `{BIN} goal activate <goal-id>`"
                    ))]);
                }
                Ok(report)
            }
            GoalAction::Next(ref id) | GoalAction::Claim(ref id) => {
                let claiming = matches!(action, GoalAction::Claim(_));
                let goal = pick_goal(id.as_deref())?;
                let absent = mdkg_graph::graph_checks::absent_pointers(&index, &goal);
                let scope = goal_scope(&index, &goal);
                let candidates = index
                    .nodes
                    .iter()
                    .filter(|n| scope.actionable.contains(&n.qid));
                coord::authority_usable(root)?;
                let prefix = Repo::open(root).map(|r| r.prefix).unwrap_or_default();
                let picked =
                    pick(&index, candidates, &config, view.as_ref(), &actor, &prefix).cloned();
                let Some(picked) = picked else {
                    return Ok(Report::ok(GoalData {
                        goal: goal_record(&goal),
                        ..GoalData::default()
                    })
                    .with(absent)
                    .with([none_ready()]));
                };
                let mut claimed = None;
                if claiming {
                    let report = coord::claim_report(root, Some(actor.clone()), &picked.qid)?;
                    claimed = Some(report);
                }
                let (node, warnings) = node_value(repo()?, &picked.qid)?;
                let (leases, diagnostics) = match claimed {
                    Some(r) => (
                        r.data.map(|d| vec![d.lease]).unwrap_or_default(),
                        r.diagnostics,
                    ),
                    None => (Vec::new(), Vec::new()),
                };
                Ok(Report::ok(GoalData {
                    goal: goal_record(&goal),
                    node: Some(node),
                    leases,
                    ..GoalData::default()
                })
                .with(absent)
                .with(diagnostics)
                .with(warnings)
                // It picks by local holders, like `next`.
                .with(coord::authority_notice(root)))
            }
            GoalAction::Activate(ref id) | GoalAction::Pause(ref id) | GoalAction::Done(ref id) => {
                let goal = pick_goal(Some(id))?;
                let s = |v: &str| FmValue::String(v.into());
                let today = today(repo()?, now_ms());
                let mut edits = Vec::new();
                let verb = match action {
                    GoalAction::Activate(_) => {
                        for other in active_goals(&index) {
                            if other.qid != goal.qid {
                                edits.push(Edit {
                                    node: other.clone(),
                                    fields: vec![
                                        ("goal_state", s("paused")),
                                        ("updated", s(&today)),
                                    ],
                                    content: None,
                                });
                            }
                        }
                        edits.push(Edit {
                            node: goal.clone(),
                            fields: vec![("goal_state", s("active")), ("updated", s(&today))],
                            content: None,
                        });
                        "activate"
                    }
                    GoalAction::Pause(_) => {
                        edits.push(Edit {
                            node: goal.clone(),
                            fields: vec![("goal_state", s("paused")), ("updated", s(&today))],
                            content: None,
                        });
                        "pause"
                    }
                    _ => {
                        edits.push(Edit {
                            node: goal.clone(),
                            fields: vec![
                                ("goal_state", s("achieved")),
                                ("status", s("done")),
                                ("updated", s(&today)),
                            ],
                            content: None,
                        });
                        "done"
                    }
                };
                let mut held = Vec::new();
                for e in &edits {
                    let leaf = repo()?.leaf(&e.node.path)?;
                    let lease =
                        coord::require_authority_hold(repo()?, &actor, &leaf, Some(&e.node.qid))?;
                    held.push((leaf, lease));
                }
                let mut store = repo()?.store()?;
                let now = now_ms();
                repo()?.observe(&mut store, now)?;
                let message = format!("{}: goal {verb}", output::display_ref(&goal.qid));
                let recorded =
                    record_edits(repo()?, &mut store, &actor, &edits, &[], &message, now)?;
                recorded.release_store_leases(&mut store, &actor, now)?;
                // Recorded: each edited node's own lease goes there too.
                released = held
                    .iter()
                    .filter_map(|(leaf, lease)| {
                        coord::release_at_authority(repo().ok()?, &actor, leaf, lease.as_ref())
                    })
                    .collect();
                let (value, warnings) = node_value(repo()?, &goal.qid)?;
                // Completing a goal anchors the store's chain at that milestone.
                let anchored =
                    (verb == "done").then(|| crate::anchor::on_goal_done(root, &mut store));
                Ok(Report::ok(GoalData {
                    goal: Some(value),
                    changesets: recorded.changesets,
                    ..GoalData::default()
                })
                .with(warnings)
                .with(anchored))
            }
        }
    })()
    .unwrap_or_else(Report::failed)
    .with(released);
    let value = |v: Option<Value>| v.unwrap_or_default();
    match name {
        "goal.current" => output::emit(
            name,
            json,
            map_report(report, |d| GoalCurrentData {
                goal: d.goal,
                leases: d.leases,
            }),
            |d| goal_text(d.goal.as_ref(), &d.leases, None),
        ),
        "goal.next" => output::emit(
            name,
            json,
            map_report(report, |d| GoalNextData {
                goal: value(d.goal),
                node: d.node,
            }),
            |d| goal_text(Some(&d.goal), &[], d.node.as_ref()),
        ),
        "goal.claim" => output::emit(
            name,
            json,
            map_report(report, |d| GoalClaimData {
                goal: value(d.goal),
                node: d.node,
                leases: d.leases,
            }),
            |d| goal_text(Some(&d.goal), &d.leases, d.node.as_ref()),
        ),
        _ => output::emit(
            name,
            json,
            map_report(report, |d| GoalChangeData {
                goal: value(d.goal),
                changesets: d.changesets,
            }),
            |d| goal_text(Some(&d.goal), &[], None),
        ),
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FormatData {
    /// Files whose frontmatter isn't in canonical form.
    pub files: Vec<String>,
    /// Whether they were rewritten (`--write`).
    pub written: bool,
    /// Those `--write` left as they were: held by another actor, or not
    /// confirmed as the actor's by the lease authority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

pub fn format(root: &Path, actor: Option<String>, ids: &[String], write: bool, json: bool) -> u8 {
    let report = (|| -> Result<Report<FormatData>, GraphError> {
        let config = config::load(root)?;
        let index = index::build(root, &config)?;
        let nodes: Vec<&IndexNode> = if ids.is_empty() {
            index.nodes.iter().collect()
        } else {
            ids.iter()
                .map(|id| noted_node(&index, id, "id"))
                .collect::<Result<_, _>>()?
        };
        // Only `--write` reads the leases: it skips files others hold.
        let view = if write { View::open(root)? } else { None };
        let repo = Repo::open(root).ok();
        let actor = repo
            .as_ref()
            .map(|r| r.actor(actor.clone()))
            .unwrap_or_else(|| actor.clone().unwrap_or_default());
        // With a lease authority the leases are there, not here: `--write`
        // writes only what the authority says the actor holds.
        let authority = match (&repo, write) {
            (Some(r), true) => crate::authority::find(r)?.is_some(),
            _ => false,
        };
        let mut files = Vec::new();
        let mut skipped = Vec::new();
        let mut authority_down = false;
        let mut store: Option<Store> = None;
        let mut diagnostics = Vec::new();
        for node in nodes {
            let path = root.join(&node.path);
            let content = index::read_document(&path)?;
            let lex = lexical(&config);
            let parsed = frontmatter::parse_with(&content, &node.path, lex)
                .map_err(|e| GraphError::from_format(&e, root))?;
            let canonical = frontmatter::write(&parsed.frontmatter, &parsed.body);
            // The canonical form must read back as the same node, or the
            // file is left as it is.
            // Key order aside: canonical form reorders keys.
            let entries = |f: &frontmatter::Frontmatter| {
                let mut e = f.entries().to_vec();
                e.sort_by(|a, b| a.0.cmp(&b.0));
                e
            };
            let same = frontmatter::parse_with(&canonical, &node.path, lex).is_ok_and(|p| {
                entries(&p.frontmatter) == entries(&parsed.frontmatter) && p.body == parsed.body
            });
            if !same {
                return Err(GraphError::failure(
                    Code::FrontmatterNotKeyValue,
                    format!(
                        "{} wouldn't read back the same in canonical form",
                        node.path
                    ),
                )
                .with_hint("edit the file by hand; format leaves it as it is"));
            }
            if canonical == content {
                continue;
            }
            files.push(node.path.clone());
            if !write {
                continue;
            }
            // A profile C node's file is shared by every worktree and kept
            // nowhere else: it's rewritten under its lease, whole,
            // and only if it still reads as format read it.
            if mdkg_graph::store_work::is_store_path(&node.path) {
                let Some(r) = repo.as_ref() else {
                    skipped.push(node.path.clone());
                    continue;
                };
                if store.is_none() {
                    store = Some(r.store()?);
                }
                let Some(store) = store.as_mut() else {
                    continue;
                };
                let now = now_ms();
                // A lease past its deadline never blocks the claim below.
                r.expire_due(store, now)?;
                let edit = Edit {
                    node: node.clone(),
                    fields: Vec::new(),
                    content: Some(canonical.clone()),
                };
                let unchanged = |_: &mut Store| -> Result<(), GraphError> {
                    if std::fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
                        Ok(())
                    } else {
                        Err(GraphError::conflict(
                            Code::CoordLeaseHeld,
                            "the file changed while format ran",
                        ))
                    }
                };
                match record_edits_checked(
                    r,
                    store,
                    &actor,
                    &[edit],
                    &[],
                    "format",
                    now,
                    &unchanged,
                    None,
                ) {
                    Ok(recorded) => {
                        // Only the leases format took: one the actor held
                        // already (here or in another worktree) stays.
                        let taken = recorded.taken.clone();
                        Recorded {
                            store_leases: recorded
                                .store_leases
                                .into_iter()
                                .filter(|l| taken.contains(l))
                                .collect(),
                            ..recorded
                        }
                        .release_store_leases(store, &actor, now)?;
                    }
                    Err(e) if e.kind == ErrorKind::Conflict => {
                        let mut d = *e.diagnostic;
                        d.severity = Severity::Warning;
                        d.message = format!("skipped: {}", d.message);
                        diagnostics.push(d.at(node.path.clone(), None));
                        skipped.push(node.path.clone());
                    }
                    Err(e) => return Err(e),
                }
                continue;
            }
            if let Some(h) = view
                .as_ref()
                .and_then(|v| v.holder(&node.path, Some(&node.qid)))
                && h.owner != actor
            {
                diagnostics.push(
                    Diagnostic::warning(Code::CoordLeaseHeld, "skipped: held by another lease")
                        .at(node.path.clone(), None)
                        .held_by([h.owner.clone()]),
                );
                skipped.push(node.path.clone());
                continue;
            }
            if let Some(r) = repo.as_ref().filter(|_| authority) {
                // Once the authority fails, the rest are skipped unasked:
                // each ask could wait out the whole timeout.
                if authority_down {
                    skipped.push(node.path.clone());
                    continue;
                }
                let held = r.leaf(&node.path).and_then(|leaf| {
                    coord::authority_exclusive_hold(r, &actor, &leaf, Some(&node.qid))
                });
                match held {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        diagnostics.push(
                            Diagnostic::warning(
                                Code::CoordAuthorityRemote,
                                "skipped: the lease authority doesn't say you hold it exclusively",
                            )
                            .at(node.path.clone(), None),
                        );
                        skipped.push(node.path.clone());
                        continue;
                    }
                    Err(e) => {
                        // A refusal is about this file; only a failure of
                        // the authority itself skips the rest unasked.
                        authority_down = e.diagnostic.code == Code::CoordAuthorityFailed;
                        let mut d = *e.diagnostic;
                        d.severity = Severity::Warning;
                        d.message = if authority_down {
                            format!("skipped, with every file after it: {}", d.message)
                        } else {
                            format!("skipped: {}", d.message)
                        };
                        diagnostics.push(d.at(node.path.clone(), None));
                        skipped.push(node.path.clone());
                        continue;
                    }
                }
            }
            crate::anchor::refuse_linked_folders(root, &node.path)?;
            std::fs::write(&path, canonical).map_err(|e| {
                GraphError::failure(
                    Code::IoWriteFailed,
                    format!("can't write {}: {e}", node.path),
                )
            })?;
        }
        Ok(Report::ok(FormatData {
            files,
            written: write,
            skipped,
        })
        .with(diagnostics))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("format", json, report, |d| {
        let verb = if d.written {
            "formatted"
        } else {
            "would format"
        };
        // File names are anyone's to choose: escape hidden characters.
        for f in &d.files {
            if d.skipped.contains(f) {
                println!("skipped {}", output::shown_path(f));
            } else {
                println!("{verb} {}", output::shown_path(f));
            }
        }
        if d.files.is_empty() {
            println!("already formatted");
        }
    })
}

/// Paths changed on this branch since main, relative to the graph root.
pub fn changed_graph_paths(root: &Path) -> Result<std::collections::BTreeSet<String>, GraphError> {
    // Read-only: a `.git` git skipped is a git problem here, not a store.
    let repo = Repo::open(root).map_err(|e| {
        if e.diagnostic.code == Code::CoordStoreFailed {
            GraphError::failure(
                Code::IoGitFailed,
                format!("can't tell which files changed: {}", e.diagnostic.message),
            )
            .with_hint(e.diagnostic.hint.clone().unwrap_or_default())
        } else {
            e
        }
    })?;
    let changed = git::changed_since(&repo.top, &coord::main_ref(&repo.top)).map_err(git_error)?;
    Ok(changed
        .into_iter()
        .filter_map(|p| p.strip_prefix(&repo.prefix).map(String::from))
        .collect())
}

/// Keeps diagnostics about changed files; drops ones with no file, as
/// 0.6.0 does for warnings.
pub fn keep_changed(
    diagnostics: Vec<Diagnostic>,
    changed: &std::collections::BTreeSet<String>,
) -> Vec<Diagnostic> {
    diagnostics
        .into_iter()
        .filter(|d| d.path.as_ref().is_some_and(|p| changed.contains(p)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_clear_is_judged_on_the_state_it_runs_against() {
        let leaf = Scope::new(".mdkg/work/task-9-x.md");
        let mut state = mdkg_coord::Coordinator::new(1_000);
        // Nobody holds a state there any more: refused, with no owner.
        assert_eq!(path_refusal(&state, true, &leaf, "a"), Some(None));
        state
            .set_work(&leaf, Some("review".into()), None, "b", 1)
            .expect("set");
        // Someone else's state is refused, naming them; your own isn't.
        assert_eq!(
            path_refusal(&state, true, &leaf, "a"),
            Some(Some("b".into()))
        );
        assert_eq!(path_refusal(&state, true, &leaf, "b"), None);
        // A clear by id (the node is in this worktree) has no owner rule.
        assert_eq!(path_refusal(&state, false, &leaf, "a"), None);
    }
}
