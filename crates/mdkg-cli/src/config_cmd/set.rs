//! `config set` and `config unset`: change one key of the committed project
//! config through mdkg's own writer.
//!
//! In ref mode the change is published on the graph ref by the graph
//! writer (`prepare_record_publish`: the exclusive lock, a pending intent
//! and its replay, the compare-and-swap with a rebuild on a newer tip). In
//! checkout mode it's committed in the checkout the committed config is
//! read from, as `agents edit` commits a definition. Either way the file
//! is held by an exclusive lease for the change, taken and released here
//! when the actor doesn't already hold one.
//!
//! A candidate config must pass the rules `doctor` and `validate` apply to
//! a committed one (the config parser, then doctor's config check), and
//! its key must be one this build knows, so nothing set here leaves a
//! config other commands refuse.

use std::path::Path;

use mdkg_format::Code;
use mdkg_format::settings::{self, Input, Layer, Origin, edit};
use mdkg_graph::GraphError;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::coord::{self, Repo};
use crate::output::Report;

/// The project config, repository-relative at the graph root.
pub(crate) const CONFIG: &str = ".mdkg/config.json";

/// What a change does to the key.
pub(crate) enum Change<'a> {
    /// Set it to this command-line value: JSON, or a bare word taken as a
    /// string.
    Set(&'a str),
    Unset,
}

/// A change of the committed config.
#[derive(Debug, Serialize, JsonSchema)]
pub struct SetData {
    /// The key as given: a dotted path or a JSON pointer.
    pub key: String,
    /// The same key as a JSON pointer.
    pub pointer: String,
    /// The committed value before the change; null when the key was
    /// absent. Credential-like values are hidden, as `config show` hides
    /// them.
    pub old: Option<Value>,
    /// The value after it; null when the key is (now) absent.
    pub new: Option<Value>,
    /// Whether the config changes. A change to the value it already has
    /// commits nothing.
    pub changed: bool,
    /// `--dry-run`: nothing was claimed, written or committed.
    pub dry_run: bool,
    /// The change to the config's JSON, as `-`/`+` lines around the
    /// changed ones, with the same values hidden.
    pub diff: String,
    /// The commit the change was read from.
    pub base: String,
    /// The commit that made the change; null for a dry run or no change.
    pub commit: Option<String>,
    /// In ref mode, the graph ref the change is published on; null in
    /// checkout mode.
    pub graph_ref: Option<String>,
    /// The config's repository-relative path.
    pub path: String,
    /// The changesets the change was recorded as.
    pub changesets: Vec<String>,
}

/// A key's object path, as `settings::edit` reads it: a JSON pointer when
/// it starts with `/`, else dotted names.
fn parts(key: &str) -> Result<Vec<String>, GraphError> {
    let fail = || {
        GraphError::failure(
            Code::ConfigInvalid,
            "the key must be a dotted object path or a JSON pointer",
        )
    };
    if key.is_empty() || key.len() > 4096 || key.chars().any(char::is_control) {
        return Err(fail());
    }
    let parts = if let Some(pointer) = key.strip_prefix('/') {
        pointer
            .split('/')
            .map(|part| {
                let mut out = String::new();
                let mut chars = part.chars();
                while let Some(c) = chars.next() {
                    out.push(if c == '~' {
                        match chars.next() {
                            Some('0') => '~',
                            Some('1') => '/',
                            _ => return Err(fail()),
                        }
                    } else {
                        c
                    });
                }
                Ok(out)
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        key.split('.').map(str::to_owned).collect()
    };
    if parts.iter().any(String::is_empty) || parts.len() > settings::MAX_DEPTH {
        return Err(fail());
    }
    Ok(parts)
}

fn pointer(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| format!("/{}", p.replace('~', "~0").replace('/', "~1")))
        .collect()
}

/// Sections whose whole content this build's readers own, so any key under
/// them is known: their readers check what they need.
const OPEN_SECTIONS: &[&str] = &[
    "affected",
    "agents",
    "archive",
    "bundles",
    "capabilities",
    "customization",
    "db",
    "index",
    "orchestration",
    "pack",
    "projections",
    "root_required",
    "subgraphs",
    "templates",
    "tool",
    "work",
    "workspaces",
];

/// `coordination`'s known settings: the mail settings, and the lease policy
/// and authority, whose parser checks what's under them.
fn known_coordination(rest: &[String]) -> bool {
    match rest.first().map(String::as_str) {
        None => true,
        Some("leases" | "lease_authority") => true,
        Some("messages") => match rest.get(1).map(String::as_str) {
            None => true,
            Some("retention" | "ack_grace_days" | "human_hold_days") => rest.len() == 2,
            Some(_) => false,
        },
        Some(_) => false,
    }
}

/// Whether `parts` names a setting this build knows: one the committed
/// config already has, one the built-in defaults have, or one in a section
/// this build reads.
fn known(parts: &[String], before: &Value, defaults: &Value) -> bool {
    let at = pointer(parts);
    if before.pointer(&at).is_some() || defaults.pointer(&at).is_some() {
        return true;
    }
    match parts[0].as_str() {
        "coordination" => known_coordination(&parts[1..]),
        top => OPEN_SECTIONS.contains(&top),
    }
}

/// Every key `value` sets at `parts`, itself included, for [`known`]: a
/// whole object set at once is checked key by key.
fn unknown_key(
    parts: &mut Vec<String>,
    value: &Value,
    before: &Value,
    defaults: &Value,
) -> Option<String> {
    if !known(parts, before, defaults) {
        return Some(parts.join("."));
    }
    if let Value::Object(map) = value {
        for (k, v) in map {
            parts.push(k.clone());
            let found = unknown_key(parts, v, before, defaults);
            parts.pop();
            if found.is_some() {
                return found;
            }
        }
    }
    None
}

fn settings_error(error: settings::Error) -> GraphError {
    GraphError::failure(
        match error.kind {
            settings::ErrorKind::Invalid => Code::ConfigInvalid,
            settings::ErrorKind::Read | settings::ErrorKind::TooLarge => Code::ConfigUnreadable,
        },
        error.to_string(),
    )
    .at(CONFIG, None)
}

/// The rules a committed config must pass: `validate`'s parser, then
/// `doctor`'s config check (the graph id, `work.committed`, the mail
/// settings' ranges, the launcher's names, the projections).
pub(crate) fn check(value: &Value) -> Result<(), GraphError> {
    let parsed =
        mdkg_graph::config::from_snapshot(value.clone()).map_err(|e| e.at(CONFIG, None))?;
    let (check, _) =
        crate::doctor::judge_config(Ok(parsed), value.clone(), Some(Ok(value.clone())), None);
    if check.status == crate::doctor::Status::Error {
        return Err(GraphError::failure(Code::ConfigInvalid, check.summary).at(CONFIG, None));
    }
    Ok(())
}

/// The run policy of every agent a change under `agents` reaches, read as
/// a supervisor reads it when it starts the agent, so a value it would
/// refuse (`env.inherit: "sometimes"`) is refused now, not at the next
/// start: `agents.defaults` (or the whole section) reaches every agent.
/// Only what the change breaks is refused: an agent whose policy was
/// already refused before it (a config broken by hand or an older build)
/// doesn't block a change, so `agents.defaults` and `agents.<name>` can
/// each be fixed while the other is still bad.
fn check_agents(parts: &[String], before: &Value, after: &Value) -> Result<(), GraphError> {
    if parts[0] != "agents" {
        return Ok(());
    }
    // The committed layer defines agents, so `agents` and each entry the
    // change touches must be objects, as the runtime's authority check
    // requires. A `null` hides an inherited agent only in the user and
    // local layers, which this command doesn't write.
    let invalid =
        |message: String| Err(GraphError::failure(Code::ConfigInvalid, message).at(CONFIG, None));
    match after.get("agents") {
        None | Some(Value::Object(_)) => {}
        Some(_) => return invalid("agents must be an object".to_owned()),
    }
    let entries = |doc: &Value| doc.get("agents").and_then(Value::as_object).cloned();
    let (old_entries, new_entries) = (entries(before), entries(after));
    for (name, entry) in new_entries.iter().flatten() {
        if name == "defaults" || entry.is_object() {
            continue;
        }
        if old_entries.as_ref().and_then(|o| o.get(name)) != Some(entry) {
            return invalid(format!("agent {name}: definition must be an object"));
        }
    }
    let mut names: Vec<String> = match parts.get(1).map(String::as_str) {
        None | Some("defaults") => after
            .get("agents")
            .and_then(Value::as_object)
            .map(|a| a.keys().filter(|k| *k != "defaults").cloned().collect())
            .unwrap_or_default(),
        Some(name) => vec![name.to_owned()],
    };
    // `defaults` alone, as an agent the config doesn't name sees it.
    names.push("defaults".to_owned());
    for name in &names {
        if let Err(e) = mdkg_run::policy::Policy::for_agent(after, name)
            && mdkg_run::policy::Policy::for_agent(before, name).is_ok()
        {
            return Err(GraphError::failure(Code::ConfigInvalid, e.message).at(CONFIG, None));
        }
    }
    Ok(())
}

/// A change worked out on the committed bytes.
pub(crate) struct Applied {
    pub(crate) parts: Vec<String>,
    /// Where `old` and `new` are read: the key's pointer.
    pub(crate) at: String,
    pub(crate) before: Value,
    pub(crate) after: Value,
    /// The file's new bytes; empty when nothing changes.
    pub(crate) bytes: Vec<u8>,
}

impl Applied {
    pub(crate) fn changed(&self) -> bool {
        self.before != self.after
    }
}

/// Works `change` out on the committed config `before`, refusing an
/// unknown key, a value that isn't valid JSON, and a result that breaks a
/// config rule.
pub(crate) fn apply(before: &[u8], key: &str, change: &Change<'_>) -> Result<Applied, GraphError> {
    let origin = Origin::file(Layer::Project, CONFIG);
    let input = Input::parse(origin.clone(), before).map_err(settings_error)?;
    let mut parts = parts(key)?;
    if matches!(
        parts[0].as_str(),
        "schema_version" | "format_version" | "graph_id"
    ) {
        return Err(GraphError::failure(
            Code::ConfigInvalid,
            format!(
                "{} is the graph's identity, which init and migrate manage",
                parts[0]
            ),
        ));
    }
    if parts[0] == "work"
        && matches!(
            parts.get(1).map(String::as_str),
            Some("location" | "committed")
        )
    {
        return Err(work_location_error());
    }
    let defaults = settings::defaults().map_err(settings_error)?.value;
    let candidate = match change {
        Change::Set(text) => {
            let value = edit::value(&origin, text).map_err(|e| {
                GraphError::failure(Code::UsageBadValue, e.to_string())
                    .with_hint("give a JSON value; quote a string that looks like a number")
            })?;
            if let Some(unknown) = unknown_key(&mut parts.clone(), &value, &input.value, &defaults)
            {
                return Err(unknown_error(&unknown));
            }
            edit::set(&input, key, value).map_err(settings_error)?.value
        }
        Change::Unset => {
            if !known(&parts, &input.value, &defaults) {
                return Err(unknown_error(&parts.join(".")));
            }
            let mut value = input.value.clone();
            let last = parts.pop().expect("a key has a name");
            if let Some(Value::Object(map)) = value.pointer_mut(&pointer(&parts)) {
                map.remove(&last);
            }
            parts.push(last);
            value
        }
    };
    // A whole `work` set or unset may not move the work either.
    if work_location(&input.value) != work_location(&candidate) {
        return Err(work_location_error());
    }
    let at = pointer(&parts);
    let applied = Applied {
        before: input.value,
        after: candidate,
        bytes: Vec::new(),
        parts,
        at,
    };
    if !applied.changed() {
        return Ok(applied);
    }
    check(&applied.after)?;
    check_agents(&applied.parts, &applied.before, &applied.after)?;
    let mut bytes = serde_json::to_vec_pretty(&applied.after)
        .map_err(|e| GraphError::failure(Code::ConfigInvalid, e.to_string()))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > settings::MAX_BYTES {
        return Err(GraphError::failure(
            Code::ConfigInvalid,
            "the changed config would exceed the 1 MiB limit",
        ));
    }
    Ok(Applied { bytes, ..applied })
}

/// Where work is kept: `work.location`, or its `work.committed` alias.
/// The storage `work` selects, with the legacy alias read as the location
/// it names (`definitions` is `git`, `none` is `local`), so a change that
/// only respells it isn't a move. An unset location stays unset: its
/// default isn't assumed here.
fn work_location(config: &Value) -> Option<Value> {
    if let Some(location) = config.pointer("/work/location") {
        return Some(location.clone());
    }
    config
        .pointer("/work/committed")
        .map(|alias| match alias.as_str() {
            Some("definitions") => Value::from("git"),
            Some("none") => Value::from("local"),
            _ => alias.clone(),
        })
}

/// Moving work between git and local storage moves its nodes too, which
/// `work localize` does; a config edit alone would hide them.
fn work_location_error() -> GraphError {
    GraphError::failure(
        Code::ConfigInvalid,
        "work.location (and its work.committed alias) says where work nodes are kept; \
         changing it alone would hide the nodes kept in the other place",
    )
    .with_hint(format!(
        "in a checkout, `{} work localize --dry-run` moves shared work out of git; \
         mdkg has no command that moves work back into git, and none that moves \
         work on a graph ref",
        crate::BIN
    ))
}

fn unknown_error(key: &str) -> GraphError {
    GraphError::failure(
        Code::ConfigInvalid,
        format!(
            "{} isn't a setting this build knows",
            crate::output::shown(key)
        ),
    )
    .with_hint(format!(
        "check the key's spelling; `{} config show` lists the settings in effect",
        crate::BIN
    ))
}

/// The change as `-`/`+` lines of the pretty-printed config, between the
/// lines both sides share, with credential-like values hidden.
pub(crate) fn diff(before: &Value, after: &Value) -> String {
    let shown = |v: &Value| {
        let mut v = v.clone();
        let _ = super::redact_display(&mut v, "");
        serde_json::to_string_pretty(&v).unwrap_or_default()
    };
    let (a, b) = (shown(before), shown(after));
    let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    if head == a.len() && head == b.len() {
        return String::new();
    }
    let mut out = format!("--- a/{CONFIG}\n+++ b/{CONFIG}\n");
    let context = 3;
    for line in &a[head.saturating_sub(context)..head] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in &a[head..a.len() - tail] {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &b[head..b.len() - tail] {
        out.push_str(&format!("+{line}\n"));
    }
    for line in &a[a.len() - tail..(a.len() - tail + context).min(a.len())] {
        out.push_str(&format!(" {line}\n"));
    }
    out
}

/// The value at `at` as `config show` shows it: the whole document is
/// redacted first, so a value hidden for its key's name (or a parent's) is
/// hidden here too.
fn shown_value(document: &Value, at: &str) -> Option<Value> {
    let mut document = document.clone();
    let renames = super::redact_display(&mut document, "");
    document.pointer(&renames.pointer(at)).cloned()
}

/// `config set` / `config unset`.
pub(crate) fn run(
    root: &Path,
    actor: Option<String>,
    key: &str,
    change: &Change<'_>,
    dry_run: bool,
) -> Result<Report<SetData>, GraphError> {
    let ref_top = super::ref_mode_root(root)?;
    if let Some(top) = &ref_top {
        super::check_ref_root_folder(top)?;
    }
    if dry_run {
        return preview(root, ref_top.as_deref(), key, change);
    }
    record(root, actor, key, change)
}

/// The report of `applied`, read at `base`; `made` is the commit and the
/// changesets that made the change, when one was made.
fn data(
    key: &str,
    applied: Applied,
    base: String,
    made: Option<(String, Vec<String>)>,
    graph_ref: Option<String>,
    path: String,
    dry_run: bool,
) -> SetData {
    let (commit, changesets) = match made {
        Some((commit, changesets)) => (Some(commit), changesets),
        None => (None, Vec::new()),
    };
    SetData {
        key: key.to_owned(),
        changed: applied.changed(),
        diff: diff(&applied.before, &applied.after),
        old: shown_value(&applied.before, &applied.at),
        new: shown_value(&applied.after, &applied.at),
        pointer: pointer(&applied.parts),
        dry_run,
        base,
        commit,
        graph_ref,
        path,
        changesets,
    }
}

/// `--dry-run`: the committed config read where every command reads it,
/// with no lock, lease, replay or write.
fn preview(
    root: &Path,
    ref_top: Option<&Path>,
    key: &str,
    change: &Change<'_>,
) -> Result<Report<SetData>, GraphError> {
    let read_at = ref_top.unwrap_or(root);
    let authority =
        mdkg_run::orchestration::config_authority(read_at).map_err(crate::run::graph_error)?;
    let (bytes, path) = match ref_top {
        Some(top) => {
            let file = mdkg_graph::committed::config_file_if_present_dir(
                top,
                &authority.commit,
                "",
                mdkg_graph::committed::DEFAULT_DIR,
            )?
            .ok_or_else(|| {
                GraphError::failure(Code::ConfigMissing, "the graph ref has no committed config")
            })?;
            (file.bytes, file.path)
        }
        None => {
            let repo = Repo::open(root)?;
            let (bytes, scope) = checkout_bytes(&repo, &authority)?;
            (bytes, scope)
        }
    };
    let applied = apply(&bytes, key, change)?;
    Ok(Report::ok(data(
        key,
        applied,
        authority.commit,
        None,
        authority.graph_ref,
        path,
        true,
    )))
}

/// Checkout mode: this checkout's committed config, which must be the one
/// every command reads (no other registered graph checkout) and must have
/// no uncommitted change, which a commit of the file would carry along.
fn checkout_bytes(
    repo: &Repo,
    authority: &mdkg_graph::graphref::Authority,
) -> Result<(Vec<u8>, String), GraphError> {
    use mdkg_coord_sqlite::git;
    let head = git::head(&repo.top).map_err(coord::git_error)?;
    if head.commit != authority.commit || !reads_here(repo, &authority.source)? {
        return Err(GraphError::failure(
            Code::ConfigInvalid,
            format!(
                "the committed config is read from {}, not this checkout's HEAD",
                crate::output::shown(&authority.source)
            ),
        )
        .with_hint(format!(
            "run `{} config set` in the registered graph checkout",
            crate::BIN
        )));
    }
    let scope = repo.leaf(CONFIG)?;
    if let Some(flag) = index_flag(&repo.top, scope.as_str())? {
        // The hint's command names the path only when it needs no shell
        // quoting (as coord's release hints do), and `<path>` otherwise;
        // the path is relative to the repository's top, where it runs.
        let path = scope.as_str();
        let arg = shell_word(path);
        return Err(GraphError::conflict(
            Code::ConfigInvalid,
            format!(
                "{} is marked {flag} in the index, so Git can't show whether it has \
                 uncommitted changes",
                crate::output::shown(path)
            ),
        )
        .with_hint(format!(
            "clear the flag from the repository's top with \
             `git update-index --no-{flag} -- {arg}`, then retry"
        )));
    }
    if !git::unchanged_since_head(&repo.top, scope.as_str()).map_err(coord::git_error)?
        || !staged_as_head(&repo.top, scope.as_str())?
    {
        return Err(GraphError::conflict(
            Code::ConfigInvalid,
            format!(
                "{} has uncommitted changes, which a commit of it would carry along",
                crate::output::shown(scope.as_str())
            ),
        )
        .with_hint("commit or restore them first"));
    }
    let path = repo.root.join(CONFIG);
    let bytes = read_regular(&path)?;
    // Git's index flags (assume-unchanged, skip-worktree) and stat cache
    // can report an edited file clean, so the working bytes must also be
    // the committed blob's, compared directly.
    let committed = mdkg_graph::committed::config_file_if_present_dir(
        &repo.top,
        &authority.commit,
        repo.prefix.trim_end_matches('/'),
        mdkg_graph::committed::DEFAULT_DIR,
    )?;
    if committed.is_none_or(|file| !same_text(&file.bytes, &bytes)) {
        return Err(GraphError::conflict(
            Code::ConfigInvalid,
            format!(
                "{} has uncommitted changes, which a commit of it would carry along",
                crate::output::shown(scope.as_str())
            ),
        )
        .with_hint("commit or restore them first"));
    }
    Ok((bytes, scope.to_string()))
}

/// Whether `working` is the committed blob as checked out: the same bytes,
/// or git's line-ending conversion of them (`core.autocrlf`, `eol=crlf`),
/// which turns each LF of an LF-only blob into CRLF.
fn same_text(committed: &[u8], working: &[u8]) -> bool {
    if committed == working {
        return true;
    }
    if committed.contains(&b'\r') {
        return false;
    }
    // The whole file converted: every LF written as CRLF, nothing else.
    let mut converted = Vec::with_capacity(committed.len() * 2);
    for &b in committed {
        if b == b'\n' {
            converted.push(b'\r');
        }
        converted.push(b);
    }
    converted == working
}

/// `path` as a shell word when it needs no quoting, and `<path>` when it
/// does, so a hint's command never runs what a folder name holds.
fn shell_word(path: &str) -> &str {
    let plain = !path.is_empty()
        && !path.starts_with('-')
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'));
    if plain { path } else { "<path>" }
}

/// Whether the index marks `path` assume-unchanged or skip-worktree. Git
/// then neither reports nor stages its working file, so a commit of it
/// would silently commit nothing.
fn index_flag(top: &Path, path: &str) -> Result<Option<&'static str>, GraphError> {
    let spec = format!(":(literal){path}");
    let out = mdkg_git::command(top)
        .args(["ls-files", "-v", "-z", "--", &spec])
        .run(None)
        .map_err(coord::git_error)?;
    if !out.status.success() {
        return Err(GraphError::failure(
            Code::ConfigUnreadable,
            format!("can't read the index entry of {path}"),
        ));
    }
    // `ls-files -v` tags a skip-worktree entry `S` and an
    // assume-unchanged one in lowercase (`s` with both set). One flag is
    // named at a time; once it is cleared, a retry names the other.
    Ok(match out.stdout.first() {
        Some(b'S') => Some("skip-worktree"),
        Some(tag) if tag.is_ascii_lowercase() => Some("assume-unchanged"),
        _ => None,
    })
}

/// Puts the index entry of `path` back to HEAD's.
fn unstage(top: &Path, path: &str) -> Result<(), String> {
    let spec = format!(":(literal){path}");
    let out = mdkg_git::command(top)
        .args(["reset", "--quiet", "HEAD", "--", &spec])
        .run(None)
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Stages `path` as `git add` would, for the `config-staged` faults only
/// (debug builds); `lock` then leaves `index.lock` behind, as a git that
/// can't write the index would.
fn stage_for_test(top: &Path, path: &str, lock: bool) {
    let spec = format!(":(literal){path}");
    let _ = mdkg_git::command(top).args(["add", "--", &spec]).run(None);
    if lock
        && let Ok(out) = mdkg_git::command(top)
            .args([
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "index.lock",
            ])
            .run(None)
    {
        let lock = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let _ = std::fs::write(lock, b"");
    }
}

/// Whether the index holds `path` as HEAD does. The working-tree check
/// alone misses a staged edit whose file was put back, which a commit of
/// the file would replace.
fn staged_as_head(top: &Path, path: &str) -> Result<bool, GraphError> {
    let spec = format!(":(literal){path}");
    let out = mdkg_git::command(top)
        .args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--cached",
            "--quiet",
            "HEAD",
            "--",
            &spec,
        ])
        .run(None)
        .map_err(coord::git_error)?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(GraphError::failure(
            Code::ConfigUnreadable,
            format!("can't tell whether {path} is staged"),
        )),
    }
}

/// Whether checkout-mode `source` is this checkout: the invoking HEAD
/// (`checkout:HEAD@…`, no registered graph checkout), or the registered
/// one (`checkout:worktrees/<admin>/HEAD@…`) when that is this worktree's
/// own git folder. A sibling at the same commit isn't it.
fn reads_here(repo: &Repo, source: &str) -> Result<bool, GraphError> {
    let Some((name, _)) = source
        .strip_prefix("checkout:")
        .and_then(|s| s.rsplit_once('@'))
    else {
        return Ok(false);
    };
    if name == "HEAD" {
        return Ok(true);
    }
    let Some(admin) = name
        .strip_prefix("worktrees/")
        .and_then(|n| n.strip_suffix("/HEAD"))
    else {
        return Ok(false);
    };
    let checkout = mdkg_git::checked_worktree(&repo.top)
        .map_err(|e| GraphError::failure(Code::ConfigUnreadable, e.to_string()))?;
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    Ok(canonical(&checkout.git_dir) == canonical(&checkout.common.join("worktrees").join(admin)))
}

/// The file's bytes, refusing a link or anything but a regular file.
fn read_regular(path: &Path) -> Result<Vec<u8>, GraphError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => (),
        Ok(_) => {
            return Err(GraphError::failure(
                Code::ConfigUnreadable,
                "the config must be a regular file",
            ));
        }
        Err(_) => {
            return Err(GraphError::failure(
                Code::ConfigMissing,
                "there is no committed config here",
            ));
        }
    }
    let bytes = mdkg_git::read_capped(path, settings::MAX_BYTES + 1)
        .map_err(|e| GraphError::failure(Code::ConfigUnreadable, e.to_string()))?;
    if bytes.len() as u64 > settings::MAX_BYTES {
        return Err(GraphError::failure(
            Code::ConfigUnreadable,
            "the config exceeds the 1 MiB limit",
        ));
    }
    Ok(bytes)
}

/// The change, made under an exclusive lease on the config: published on
/// the graph ref in ref mode, committed in checkout mode.
fn record(
    root: &Path,
    actor: Option<String>,
    key: &str,
    change: &Change<'_>,
) -> Result<Report<SetData>, GraphError> {
    use crate::coord::{now_ms, store_error};
    use mdkg_coord::{Command, Mode, Outcome};

    let bound = coord::graph_git::bind(root)?;
    let root = bound.as_deref().unwrap_or(root);
    crate::graph_repair::require_writable(root)?;
    let repo = Repo::open(root)?;
    let ref_mode = coord::graph_git::for_root(&repo.root);
    crate::anchor::refuse_linked_folders(&repo.root, CONFIG)?;
    let scope = repo.leaf(CONFIG)?;
    coord::check_scope_text(&scope)?;
    let actor = repo.actor(actor);
    crate::msg::check_actor_mailbox(&actor, true)?;
    // Config dispatch runs before the CLI's own actor gate, so a human,
    // role, turn or mdkg name is refused here, before anything is written.
    crate::msg::check_actor(&actor)?;
    // Checkout mode: checked before any lease is taken.
    let checkout = match &ref_mode {
        Some(_) => None,
        None => {
            let authority = mdkg_run::orchestration::config_authority(&repo.root)
                .map_err(crate::run::graph_error)?;
            let (bytes, _) = checkout_bytes(&repo, &authority)?;
            Some((authority, bytes))
        }
    };
    let held = coord::require_authority_hold(&repo, &actor, &scope, None)?;
    let mut store = repo.store()?;
    let now = now_ms();
    repo.observe(&mut store, now)?;
    let state = store.coordinator().map_err(store_error)?;
    let existing = coord::covering_lease(&state, &actor, &scope);
    let taken = existing.is_none();
    let lease = if let Some(lease) = existing {
        if lease.scope != scope || lease.mode != Mode::Exclusive {
            return Err(GraphError::conflict(
                Code::CoordLeaseHeld,
                "a config change needs an exclusive lease on the config file",
            )
            .with_hint("release the covering or shared lease, then retry"));
        }
        lease.id.clone()
    } else {
        let mut capped = false;
        let outcome = store.execute_with(|state| {
            if coord::at_lease_cap(state, &actor, now) {
                capped = true;
                return Err(mdkg_coord_sqlite::StoreError::Schema("capped".into()));
            }
            Ok(Command::Claim {
                owner: actor.clone(),
                scope: scope.clone(),
                mode: Mode::Exclusive,
                now,
                request: None,
                node: None,
            })
        });
        if capped {
            return Err(coord::lease_cap_error());
        }
        match outcome.map_err(store_error)? {
            Outcome::Lease { lease } => lease,
            Outcome::Refused { error } => return Err(coord::refusal(&error)),
            _ => {
                return Err(GraphError::failure(
                    Code::CoordLeaseHeld,
                    "the config's lease wasn't granted",
                ));
            }
        }
    };
    let release = |store: &mut mdkg_coord_sqlite::Store| -> Result<(), GraphError> {
        // `config-release` stands in for a release the store refuses.
        crate::write::fault("config-release")?;
        let outcome = store
            .execute(&Command::Release {
                owner: actor.clone(),
                lease: lease.clone(),
                now,
            })
            .map_err(store_error)?;
        if let Outcome::Refused { error } = outcome {
            return Err(coord::refusal(&error));
        }
        Ok(())
    };
    let message = match change {
        Change::Set(_) => format!("config set {key}"),
        Change::Unset => format!("config unset {key}"),
    };
    let result = (|| {
        let report = match &ref_mode {
            Some(mode) => {
                use crate::coord::graph_git::{
                    Publish, Wrote, next_request_id, prepare_record_publish,
                };
                let graph = mode.graph.name.clone();
                let mut changesets = Vec::new();
                let mut base = String::new();
                let wrote = prepare_record_publish(
                    mode,
                    &mut store,
                    (&next_request_id(), "config", &actor),
                    |w, _| {
                        // The cache is at the tip, under the exclusive lock.
                        base = w.base().to_owned();
                        let applied = apply(&read_regular(&repo.root.join(CONFIG))?, key, change)?;
                        if applied.changed() {
                            w.write(scope.as_str(), &applied.bytes)?;
                        }
                        crate::write::fault("config-written")?;
                        Ok((
                            Publish {
                                message: message.clone(),
                                leases: vec![lease.to_string()],
                                allowed: vec![scope.clone()],
                                trailers: Vec::new(),
                            },
                            applied,
                        ))
                    },
                    |store, prepared| {
                        let source = mdkg_coord::Source {
                            commit: prepared.commit.clone(),
                            branch: graph.clone(),
                        };
                        changesets = crate::write::record_changesets(
                            &repo,
                            store,
                            &actor,
                            &[(lease.to_string(), scope.clone())],
                            &[],
                            &source,
                            now,
                            None,
                            None,
                        )?;
                        Ok(())
                    },
                )?;
                match wrote {
                    Wrote::Done(applied, Some(written)) => data(
                        key,
                        applied,
                        base,
                        Some((written.receipt.commit, changesets)),
                        Some(graph),
                        scope.to_string(),
                        false,
                    ),
                    Wrote::Done(applied, None) => {
                        // Only a lease this command took: one the actor
                        // held before stays theirs.
                        if taken {
                            release(&mut store)?;
                        }
                        data(
                            key,
                            applied,
                            base,
                            None,
                            Some(graph),
                            scope.to_string(),
                            false,
                        )
                    }
                    // Request ids are minted per command, so a replay of
                    // this one can't be found here.
                    Wrote::Already(_) => {
                        return Err(GraphError::failure(
                            Code::IoWriteFailed,
                            "this change's request was already published",
                        ));
                    }
                }
            }
            None => {
                let (authority, bytes) = checkout.expect("checkout mode read its config");
                let applied = apply(&bytes, key, change)?;
                let path = repo.root.join(CONFIG);
                if !applied.changed() {
                    if taken {
                        release(&mut store)?;
                    }
                    return Ok(Report::ok(data(
                        key,
                        applied,
                        authority.commit,
                        None,
                        None,
                        scope.to_string(),
                        false,
                    )));
                }
                // `config-renamed` stands in for a failure after the
                // rename (syncing the folder), so the restore is tested.
                let written = crate::write::atomic_write(&path, &applied.bytes)
                    .map_err(|e| e.to_string())
                    .and_then(|()| {
                        crate::write::fault("config-renamed")
                            .map_err(|e| e.diagnostic.message.clone())
                    });
                if let Err(e) = written {
                    // The rename may have landed before a later step
                    // (syncing the folder) failed: put the committed bytes
                    // back whenever the file no longer holds them.
                    let error = format!("writing the config: {e}");
                    if read_regular(&path).ok().as_deref() != Some(bytes.as_slice())
                        && let Err(restore) = crate::write::atomic_write(&path, &bytes)
                    {
                        return Err(GraphError::failure(
                            Code::IoWriteFailed,
                            format!("{error}; restoring the config failed: {restore}"),
                        ));
                    }
                    return Err(GraphError::failure(Code::IoWriteFailed, error));
                }
                let mut committed = false;
                let recorded = crate::write::fault("config-written").and_then(|()| {
                    // `config-staged` stands in for a failure after git
                    // staged the file (its `diff --cached`, say), so the
                    // unstaging below is tested.
                    if let Err(e) = crate::write::fault("config-staged") {
                        stage_for_test(&repo.top, scope.as_str(), false);
                        return Err(e);
                    }
                    // `config-staged-locked` also leaves the index locked,
                    // so the unstaging fails and the restore still runs.
                    if let Err(e) = crate::write::fault("config-staged-locked") {
                        stage_for_test(&repo.top, scope.as_str(), true);
                        return Err(e);
                    }
                    crate::write::commit_and_record(
                        &repo,
                        &mut store,
                        &actor,
                        &[(lease.to_string(), scope.clone())],
                        &[],
                        &message,
                        now,
                        None,
                        None,
                    )
                    .map_err(|(error, done)| {
                        // A commit is never undone, even when the store
                        // couldn't record it.
                        committed = done;
                        error
                    })
                });
                // A commit that left HEAD where it was committed nothing
                // (git didn't stage the file): never reported as made.
                let recorded = recorded.and_then(|r| {
                    if r.commit == authority.commit {
                        // Nothing landed, so the working file is restored.
                        Err(GraphError::failure(
                            Code::IoWriteFailed,
                            "git committed nothing for the config change",
                        ))
                    } else {
                        Ok(r)
                    }
                });
                match recorded {
                    Ok(r) => data(
                        key,
                        applied,
                        authority.commit,
                        Some((r.commit, r.changesets)),
                        None,
                        scope.to_string(),
                        false,
                    ),
                    Err(error) => {
                        // Nothing may stay staged to slip into a later
                        // commit: the index held HEAD's entry before (the
                        // checks above), so it is put back to HEAD's.
                        // Both are tried, and both failures reported.
                        if !committed {
                            let mut failed = Vec::new();
                            if let Err(unstage) = unstage(&repo.top, scope.as_str()) {
                                failed.push(format!("unstaging the config failed: {unstage}"));
                            }
                            if let Err(restore) = crate::write::atomic_write(&path, &bytes) {
                                failed.push(format!("restoring the config failed: {restore}"));
                            }
                            if !failed.is_empty() {
                                return Err(GraphError::failure(
                                    Code::IoWriteFailed,
                                    format!("{}; {}", error.diagnostic.message, failed.join("; ")),
                                ));
                            }
                        }
                        return Err(error);
                    }
                }
            }
        };
        // The authority's lease is released only when the change was
        // committed, as other recorded work releases it: a no-op leaves
        // the actor holding it.
        let released = if report.commit.is_some() {
            coord::release_at_authority(&repo, &actor, &scope, held.as_ref())
        } else {
            None
        };
        Ok(Report::ok(report).with(released))
    })();
    match result {
        // A lease that couldn't be released may still be held: named by
        // its id, which `status` lists. No release command is suggested:
        // beside a lease authority, a release by path reaches the
        // authority's lease, not this clone's.
        Err(mut error) if taken => {
            if let Err(release) = release(&mut store) {
                error.diagnostic.message = format!(
                    "{}; releasing this command's lease {lease} on the config failed, so it \
                     may still be held in this clone's coordinator until it expires: {}",
                    error.diagnostic.message, release.diagnostic.message
                );
            }
            Err(error)
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_hint_names_only_a_path_that_needs_no_quoting() {
        assert_eq!(shell_word(".mdkg/config.json"), ".mdkg/config.json");
        assert_eq!(
            shell_word("services/api/.mdkg/config.json"),
            "services/api/.mdkg/config.json"
        );
        for odd in [
            "pkg;touch x;#/.mdkg/config.json",
            "a b/.mdkg/config.json",
            "$(x)/c",
            "-x",
            "",
        ] {
            assert_eq!(shell_word(odd), "<path>", "{odd}");
        }
    }

    fn config() -> Vec<u8> {
        serde_json::to_vec_pretty(&json!({
            "schema_version": 3,
            "format_version": "0.7",
            "graph_id": "0192f3a0-0000-7000-8000-000000000000",
            "coordination": {"messages": {"ack_grace_days": 7}}
        }))
        .unwrap()
    }

    #[test]
    fn a_crlf_checkout_of_an_lf_blob_is_the_same_text() {
        assert!(same_text(b"{\n}\n", b"{\r\n}\r\n"));
        assert!(same_text(b"{\n}\n", b"{\n}\n"));
        assert!(!same_text(b"{\n}\n", b"{\r\n }\r\n"));
        assert!(!same_text(b"{\r\n}\n", b"{\r\n}\r\n"));
        assert!(!same_text(b"{\n}\n", b"{\r}\n"));
        // Mixed endings aren't a whole-file conversion.
        assert!(!same_text(b"{\n}\n", b"{\r\n}\n"));
    }

    #[test]
    fn a_set_changes_one_key_and_keeps_the_rest() {
        let a = apply(
            &config(),
            "coordination.messages.ack_grace_days",
            &Change::Set("0"),
        )
        .unwrap();
        assert!(a.changed());
        assert_eq!(a.before.pointer(&a.at), Some(&json!(7)));
        assert_eq!(a.after.pointer(&a.at), Some(&json!(0)));
        assert_eq!(a.after["graph_id"], "0192f3a0-0000-7000-8000-000000000000");
        let d = diff(&a.before, &a.after);
        assert!(d.contains("-      \"ack_grace_days\": 7"), "{d}");
        assert!(d.contains("+      \"ack_grace_days\": 0"), "{d}");
        let p = apply(
            &config(),
            "/coordination/messages/ack_grace_days",
            &Change::Set("7"),
        )
        .unwrap();
        assert!(!p.changed(), "the same value changes nothing");
        assert!(diff(&p.before, &p.after).is_empty());
    }

    #[test]
    fn out_of_range_unknown_and_identity_keys_are_refused() {
        for (key, value) in [
            ("coordination.messages.ack_grace_days", "3651"),
            ("coordination.messages.ack_grace_days", "-1"),
            ("coordination.messages.human_hold_days", "0"),
            ("coordination.messages.retention", "\"forever\""),
            ("coordination.leases.default", "\"advisory\""),
            ("coordination.mesages.ack_grace_days", "1"),
            ("coordination.messages.ack_grace", "1"),
            ("nonsense", "1"),
            ("graph_id", "\"0192f3a0-0000-7000-8000-000000000001\""),
            ("coordination.messages", "{\"ack_grace\": 1}"),
            ("coordination.messages.ack_grace_days", "{"),
        ] {
            let e = apply(&config(), key, &Change::Set(value))
                .err()
                .unwrap_or_else(|| panic!("{key} = {value} must be refused"));
            assert!(
                matches!(e.diagnostic.code, Code::ConfigInvalid | Code::UsageBadValue),
                "{key} = {value}: {:?}",
                e.diagnostic
            );
        }
    }

    #[test]
    fn an_agent_policy_the_supervisor_would_refuse_is_refused_at_set_time() {
        for (key, value) in [
            ("agents.defaults.env.inherit", "\"sometimes\""),
            ("agents.defaults.env", "{\"inherit\": true, \"extra\": 1}"),
            ("agents.worker.policy.env.inherit", "\"yes\""),
            ("agents", "{\"defaults\": {\"env\": {\"inherit\": 2}}}"),
        ] {
            let e = apply(&config(), key, &Change::Set(value))
                .err()
                .unwrap_or_else(|| panic!("{key} = {value} must be refused"));
            assert_eq!(e.diagnostic.code, Code::ConfigInvalid, "{key}");
            assert!(
                e.diagnostic.message.contains("env"),
                "{key}: {}",
                e.diagnostic.message
            );
        }
        let ok = apply(
            &config(),
            "agents.defaults.env.inherit",
            &Change::Set("true"),
        )
        .unwrap();
        assert!(ok.changed());
    }

    #[test]
    fn the_work_location_is_refused_by_key_and_by_a_whole_work_change() {
        let local = serde_json::to_vec(&json!({
            "schema_version": 3, "format_version": "0.7",
            "graph_id": "0192f3a0-0000-7000-8000-000000000000",
            "work": {"location": "local", "other": 1}
        }))
        .unwrap();
        for key in ["work.location", "work.committed"] {
            for change in [
                Change::Set("\"git\""),
                Change::Set("\"local\""),
                Change::Unset,
            ] {
                let e = apply(&local, key, &change).err().expect("refused");
                assert!(e.diagnostic.message.contains("work.location"), "{key}");
                let hint = e.diagnostic.hint.as_deref().unwrap();
                assert!(hint.contains("work localize"), "{hint}");
                assert!(hint.contains("back into git") && hint.contains("graph ref"));
            }
        }
        assert!(apply(&local, "work", &Change::Unset).is_err());
        assert!(apply(&local, "work", &Change::Set("{\"location\": \"git\"}")).is_err());
        assert!(apply(&config(), "work", &Change::Set("{\"committed\": \"none\"}")).is_err());
        // A whole `work` set that keeps the location goes through.
        let kept = apply(&local, "work", &Change::Set("{\"location\": \"local\"}")).unwrap();
        assert!(kept.changed());
    }

    #[test]
    fn unset_removes_the_key_and_an_absent_key_changes_nothing() {
        let a = apply(
            &config(),
            "coordination.messages.ack_grace_days",
            &Change::Unset,
        )
        .unwrap();
        assert!(a.changed());
        assert_eq!(a.before.pointer(&a.at), Some(&json!(7)));
        assert_eq!(a.after.pointer(&a.at), None);
        assert_eq!(a.after["coordination"]["messages"], json!({}));
        let b = apply(
            &config(),
            "coordination.messages.human_hold_days",
            &Change::Unset,
        )
        .unwrap();
        assert!(!b.changed());
        assert!(apply(&config(), "coordination.bogus", &Change::Unset).is_err());
        assert!(apply(&config(), "graph_id", &Change::Unset).is_err());
    }

    #[test]
    fn diff_and_values_hide_credentials() {
        let a = apply(
            &config(),
            "customization.api_token",
            &Change::Set(concat!("\"sk-", "live-", "abcdefghijklmnopqrstuvwxyz\"")),
        )
        .expect("an open section takes the key");
        let d = diff(&a.before, &a.after);
        assert!(d.contains("+    \"api_token\""), "{d}");
        assert!(!d.contains("abcdefghijklmnop"), "{d}");
        let shown = shown_value(&a.after, &a.at).unwrap();
        assert!(!shown.to_string().contains("abcdefghijklmnop"), "{shown}");
    }

    /// A plain value is hidden for its key's name, or a parent's, as
    /// `config show` hides it, in both `old` and `new`.
    #[test]
    fn values_hidden_by_their_key_name_stay_hidden() {
        let mut before: Value = serde_json::from_slice(&config()).unwrap();
        before["customization"] = json!({"password": "plain words", "secrets": {"x": "plain"}});
        let before = serde_json::to_vec(&before).unwrap();
        for key in ["customization.password", "customization.secrets.x"] {
            let a = apply(&before, key, &Change::Unset).unwrap();
            assert_eq!(
                shown_value(&a.before, &a.at),
                Some(json!("[redacted]")),
                "{key}"
            );
            assert_eq!(shown_value(&a.after, &a.at), None, "{key}");
            assert!(!diff(&a.before, &a.after).contains("plain"), "{key}");
            let b = apply(&before, key, &Change::Set("\"other words\"")).unwrap();
            assert_eq!(
                shown_value(&b.after, &b.at),
                Some(json!("[redacted]")),
                "{key}"
            );
        }
    }

    /// A bad `agents.defaults` and a bad `agents.x` don't block each
    /// other's fix; a change that breaks a good policy is still refused.
    #[test]
    fn only_a_policy_the_change_breaks_is_refused() {
        let parts = |k: &str| k.split('.').map(str::to_owned).collect::<Vec<_>>();
        let bad = json!({"agents": {
            "defaults": {"env": {"inherit": "sometimes"}},
            "x": {"policy": {"env": {"inherit": "yes"}}}
        }});
        let mut fix_x = bad.clone();
        fix_x["agents"]["x"]["policy"]["env"]["inherit"] = json!(true);
        assert!(check_agents(&parts("agents.x"), &bad, &fix_x).is_ok());
        let mut fix_defaults = bad.clone();
        fix_defaults["agents"]["defaults"]["env"]["inherit"] = json!(false);
        assert!(check_agents(&parts("agents.defaults"), &bad, &fix_defaults).is_ok());
        let mut unset_x = bad.clone();
        unset_x["agents"].as_object_mut().unwrap().remove("x");
        assert!(check_agents(&parts("agents.x"), &bad, &unset_x).is_ok());

        let good = json!({"agents": {"x": {"policy": {"env": {"inherit": true}}}}});
        let mut break_x = good.clone();
        break_x["agents"]["x"]["policy"]["env"]["inherit"] = json!("yes");
        assert!(check_agents(&parts("agents.x"), &good, &break_x).is_err());
        let mut break_defaults = good.clone();
        break_defaults["agents"]["defaults"] = json!({"env": {"inherit": "sometimes"}});
        assert!(check_agents(&parts("agents.defaults"), &good, &break_defaults).is_err());
    }

    /// Respelling the legacy alias as the location it names isn't a move;
    /// changing the storage is.
    #[test]
    fn the_work_guard_compares_the_storage_selected() {
        let legacy = json!({"work": {"committed": "definitions"}});
        assert_eq!(
            work_location(&legacy),
            work_location(&json!({"work": {"location": "git"}}))
        );
        assert_eq!(
            work_location(&json!({"work": {"committed": "none"}})),
            work_location(&json!({"work": {"location": "local"}}))
        );
        assert_ne!(
            work_location(&legacy),
            work_location(&json!({"work": {"location": "local"}}))
        );
        assert_ne!(work_location(&legacy), work_location(&json!({})));
    }

    /// Through `apply`: a whole `work` set that respells the legacy alias
    /// goes through, one that changes the storage is refused.
    #[test]
    fn a_whole_work_set_may_respell_the_alias_but_not_move_the_work() {
        let mut legacy: Value = serde_json::from_slice(&config()).unwrap();
        legacy["work"] = json!({"committed": "definitions"});
        let legacy = serde_json::to_vec_pretty(&legacy).unwrap();
        let applied = apply(&legacy, "work", &Change::Set(r#"{"location":"git"}"#))
            .unwrap_or_else(|e| panic!("{}", e.diagnostic.message));
        assert!(applied.changed());
        assert_eq!(applied.after["work"], json!({"location": "git"}));
        for change in [Change::Set(r#"{"location":"local"}"#), Change::Unset] {
            let e = apply(&legacy, "work", &change).err().expect("refused");
            assert!(
                e.diagnostic.message.contains("work.location"),
                "{}",
                e.diagnostic.message
            );
        }
    }

    /// A key that looks like a credential is renamed in the diff and in
    /// `old`/`new`, as `config show` renames it, and the value under it is
    /// still found and hidden.
    #[test]
    fn a_key_that_looks_like_a_credential_is_renamed_in_the_diff_and_values() {
        let key = concat!("https://someone:SYNTHETIC_KEY_PASS", "@example.test/x");
        let before = json!({"mirrors": {}});
        let after = json!({"mirrors": {key: {"note": "plain"}}});
        let text = diff(&before, &after);
        assert!(!text.contains("SYNTHETIC_"), "{text}");
        assert!(text.contains("[redacted key 1]"), "{text}");
        let at = format!("/mirrors/{}", key.replace('~', "~0").replace('/', "~1"));
        assert_eq!(
            shown_value(&after, &at),
            Some(json!({"note": "[redacted]"}))
        );
        assert_eq!(
            shown_value(&after, &format!("{at}/note")),
            Some(json!("[redacted]"))
        );
    }
}
