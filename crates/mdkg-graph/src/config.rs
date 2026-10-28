//! `.mdkg/config.json`: only the fields phase 1 needs, with the reference
//! implementation's defaults. Unknown fields are ignored, not rejected.

use std::path::Path;

use serde_json::Value;

use mdkg_format::Code;
use mdkg_format::Uuid7;
use mdkg_format::node::FormatVersion;

use crate::error::GraphError;

pub mod layers;

pub const CONFIG_PATH: &str = ".mdkg/config.json";

/// The newest `.mdkg/config.json` schema this build reads (orchestrator,
/// 2026-10-21). This is the config file's version, not the coordinator
/// store's schema. 3 is format 0.7 (`docs/format-0.7.md`, section 2); 2,
/// 0.6.0's identity format v2, is refused.
pub const MAX_SCHEMA_VERSION: i64 = 3;

/// The config `schema_version` of a format 0.7 graph.
pub const V07_SCHEMA_VERSION: i64 = 3;

/// The `format_version` a format 0.7 graph declares.
pub const FORMAT_VERSION_07: &str = "0.7";

/// What happened to a graph mdkg refuses as identity format v2, and
/// how to bring it forward or get it back (v2 graphs
/// migrate keeping their UUIDs). mdkg 0.6.0's `graph
/// migrate --apply` writes config `schema_version: 2`, `.mdkg/graph.json`
/// and identity fields in every node, in every workspace; a v2 `graph clone`
/// or `fork` writes graph.json too; `upgrade` writes 2 only on a graph that
/// already has that file. `init` never writes 2.
/// The 0.6.0 commands keep 0.6.0's name; `migrate` is this build's, named
/// as it runs.
pub fn identity_v2_hint() -> String {
    format!(
        "this graph is in identity format v2, written by mdkg 0.6.0 (`mdkg graph migrate --apply`, or a v2 `graph clone`/`fork`); run `{} migrate --dry-run`, then `--apply`, to bring it to format 0.7, keeping its graph and node UUIDs. If nothing has changed since the identity migration, 0.6.0's `mdkg graph recover` can still roll it back in two steps",
        mdkg_format::program::BIN
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub alias: String,
    pub path: String,
    pub enabled: bool,
    pub mdkg_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_files: u64,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_depth: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub tool: String,
    pub schema_version: i64,
    /// Format 0.7 for `schema_version` 3, otherwise 0.6.0's.
    pub format: FormatVersion,
    /// A format 0.7 graph's `graph_id`.
    pub graph_id: Option<Uuid7>,
    pub status_enum: Vec<String>,
    pub priority_min: i64,
    pub priority_max: i64,
    /// Sorted by alias in code-unit order, as the reference indexes them.
    pub workspaces: Vec<Workspace>,
    /// The skill mirrors: a config's `projections` (its `skills:<folder>`
    /// entries) when it has the key, else `customization.skill_mirrors.targets`.
    pub skill_mirror_targets: Vec<String>,
    pub templates_root: String,
    pub templates_set: String,
    pub limits: Limits,
    pub tolerant: bool,
    /// Registered read-only subgraph aliases (not loaded in phase 1).
    pub subgraphs: Vec<String>,
    /// `work.next.status_preference`: the statuses `next` picks from, best
    /// first.
    pub next_status_preference: Vec<String>,
    /// `coordination.leases`: which paths may be leased by several actors.
    pub leases: LeasePolicy,
    /// `work.location` (`work.committed` is a compatibility alias): whether
    /// work-type nodes live in git or beside the store.
    pub work_committed: WorkCommitted,
    /// Under `work.committed: none`, the folder beside the coordinator
    /// store holding this graph's work-type nodes (`store_work`); `None`
    /// otherwise, or outside a git repository.
    pub store_work_dir: Option<std::path::PathBuf>,
}

/// Work storage, including the legacy `work.committed` spellings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkCommitted {
    /// Profile B, the default: work nodes are committed in `.mdkg/work`.
    #[default]
    Definitions,
    /// Profile C: work-type nodes live beside the coordinator store, which
    /// git never sees, in the same file format.
    None,
}

impl WorkCommitted {
    /// The canonical config spelling for this storage choice.
    pub fn location(self) -> &'static str {
        match self {
            Self::Definitions => "git",
            Self::None => "local",
        }
    }
}

/// Whether a path's lease is exclusive or shared by several actors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LeaseMode {
    #[default]
    Exclusive,
    Shared,
}

/// What the commit-time guard does with a staged path the actor holds no
/// lease on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Unleased {
    #[default]
    Warn,
    Block,
}

/// `coordination.leases` in the config:
///
/// ```json
/// {"coordination": {"leases": {
///   "default": "exclusive",
///   "paths": [{"glob": "go.sum", "mode": "shared"}],
///   "unleased": "warn"
/// }}}
/// ```
///
/// Globs are relative to the graph root and match whole `/`-separated
/// segments: `*` and `?` stay inside one segment, `**` spans any number of
/// them. Case never matters, as for lease scopes. The most specific match
/// wins: the most segments without a wildcard, then the most segments other
/// than `**`, then the earliest entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeasePolicy {
    pub default: LeaseMode,
    pub paths: Vec<(String, LeaseMode)>,
    pub unleased: Unleased,
}

/// The lowercase form lease scopes compare by.
fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| if c == 'ς' { 'σ' } else { c })
        .collect()
}

fn segments(s: &str) -> Vec<String> {
    s.split('/')
        .filter(|x| !x.is_empty() && *x != ".")
        .map(fold)
        .collect()
}

/// `*` and `?` inside one segment.
fn segment_matches(glob: &[char], name: &[char]) -> bool {
    match (glob.first(), name.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            segment_matches(&glob[1..], name)
                || (!name.is_empty() && segment_matches(glob, &name[1..]))
        }
        (Some('?'), Some(_)) => segment_matches(&glob[1..], &name[1..]),
        (Some(g), Some(n)) if g == n => segment_matches(&glob[1..], &name[1..]),
        _ => false,
    }
}

/// Whether `glob` matches `path` exactly (`whole`), or could match
/// something below it (`!whole`, `path` read as a folder).
fn glob_matches(glob: &[String], path: &[String], whole: bool) -> bool {
    match (glob.first(), path.first()) {
        // A trailing `**` matches anything below a folder that's used up.
        (Some(g), _) if g == "**" => {
            (path.is_empty() && !whole)
                || glob_matches(&glob[1..], path, whole)
                || (!path.is_empty() && glob_matches(glob, &path[1..], whole))
        }
        (None, None) => whole,
        (Some(_), None) => !whole,
        (None, Some(_)) => false,
        (Some(g), Some(n)) => {
            let g: Vec<char> = g.chars().collect();
            let n: Vec<char> = n.chars().collect();
            segment_matches(&g, &n) && glob_matches(&glob[1..], &path[1..], whole)
        }
    }
}

/// How specific a glob is: segments without a wildcard, then segments
/// other than `**`.
fn rank(glob: &[String]) -> (usize, usize) {
    let literal = glob.iter().filter(|g| !g.contains(['*', '?'])).count();
    let bounded = glob.iter().filter(|g| *g != "**").count();
    (literal, bounded)
}

impl LeasePolicy {
    /// The mode a claim on `path` (relative to the graph root) gets.
    /// Anything overlapping a folder that holds nodes (`graph_dirs`) is
    /// exclusive whatever the config says. A folder is shared only when
    /// everything inside it would be: its shared mode (from a glob, or the
    /// default) covers what's below (a glob ending in `/**`, or a shared
    /// default), and no exclusive entry at least as specific could match
    /// anything inside it. So a shared folder lease never covers a file that
    /// would be exclusive on its own.
    pub fn mode_for(&self, path: &str, graph_dirs: &[String], folder: bool) -> LeaseMode {
        let path = segments(path);
        let overlaps_graph = graph_dirs.iter().any(|dir| {
            let graph = segments(dir);
            path.iter().zip(&graph).all(|(a, b)| a == b)
        });
        if overlaps_graph {
            return LeaseMode::Exclusive;
        }
        let globs: Vec<(Vec<String>, LeaseMode)> =
            self.paths.iter().map(|(g, m)| (segments(g), *m)).collect();
        let best = globs
            .iter()
            .filter(|(g, _)| glob_matches(g, &path, true))
            .min_by_key(|(g, _)| std::cmp::Reverse(rank(g)));
        let mode = best.map_or(self.default, |(_, m)| *m);
        if mode == LeaseMode::Exclusive || !folder {
            return mode;
        }
        // A shared glob ending in `/**` matches everything inside, so only an
        // exclusive glob at least as specific can beat it there. Without one,
        // what's inside falls to other globs or the default, and any
        // exclusive glob that could match inside counts.
        let below = best
            .filter(|(g, _)| g.last().is_some_and(|s| s == "**"))
            .map(|(g, _)| rank(g));
        let covers_below = below.is_some() || self.default == LeaseMode::Shared;
        let exclusive_below = globs.iter().any(|(g, m)| {
            *m == LeaseMode::Exclusive
                && below.is_none_or(|b| rank(g) >= b)
                && glob_matches(g, &path, false)
        });
        if covers_below && !exclusive_below {
            LeaseMode::Shared
        } else {
            LeaseMode::Exclusive
        }
    }
}

impl Config {
    /// The lease mode for `path` (relative to the graph root), with every
    /// workspace's node folder kept exclusive.
    pub fn lease_mode(&self, path: &str, folder: bool) -> LeaseMode {
        let mut dirs = vec![".mdkg".to_string()];
        dirs.extend(
            self.workspaces
                .iter()
                .map(|w| format!("{}/{}", w.path, w.mdkg_dir)),
        );
        self.leases.mode_for(path, &dirs, folder)
    }
}

fn mode_named(name: &str) -> Option<LeaseMode> {
    match name {
        "exclusive" => Some(LeaseMode::Exclusive),
        "shared" => Some(LeaseMode::Shared),
        _ => None,
    }
}

fn lease_policy(v: &Value, errors: &mut Vec<String>) -> LeasePolicy {
    let Some(leases) = v.pointer("/coordination/leases") else {
        return LeasePolicy::default();
    };
    let mut policy = LeasePolicy::default();
    match leases
        .get("default")
        .map(|d| d.as_str().and_then(mode_named))
    {
        None => {}
        Some(Some(m)) => policy.default = m,
        Some(None) => {
            errors.push("coordination.leases.default must be \"exclusive\" or \"shared\"".into())
        }
    }
    match leases.get("unleased").map(Value::as_str) {
        None => {}
        Some(Some("warn")) => policy.unleased = Unleased::Warn,
        Some(Some("block")) => policy.unleased = Unleased::Block,
        Some(_) => {
            errors.push("coordination.leases.unleased must be \"warn\" or \"block\"".into());
        }
    }
    match leases.get("paths") {
        None => {}
        Some(Value::Array(entries)) => {
            for (i, entry) in entries.iter().enumerate() {
                let glob = str_at(entry, &["glob"]).filter(|g| !segments(g).is_empty());
                let mode = str_at(entry, &["mode"]).and_then(mode_named);
                match (glob, mode) {
                    (Some(g), Some(m)) => policy.paths.push((g.to_string(), m)),
                    _ => errors.push(format!(
                        "coordination.leases.paths[{i}] needs a non-empty \"glob\" and a \"mode\" of \"exclusive\" or \"shared\""
                    )),
                }
            }
        }
        Some(_) => errors.push("coordination.leases.paths must be a list".into()),
    }
    policy
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_str()
}

fn int_at(v: &Value, path: &[&str]) -> Option<i64> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_i64()
}

fn bool_at(v: &Value, path: &[&str]) -> Option<bool> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_bool()
}

fn is_alias(alias: &str) -> bool {
    mdkg_format::ids::is_workspace_alias(alias)
}

/// The parts of a workspace path, split on either slash, as 0.6.0 splits
/// them.
fn path_parts(value: &str) -> impl Iterator<Item = &str> {
    mdkg_format::js::trim(value)
        .split(['/', '\\'])
        .filter(|p| !p.is_empty())
}

/// Why a workspace path doesn't stay inside the repository, if it doesn't:
/// it must be non-empty and relative, with no `..` (0.6.0
/// `normalizeContainedWorkspacePath`).
fn contained_path_error(value: &str) -> Option<&'static str> {
    let v = mdkg_format::js::trim(value);
    let b = v.as_bytes();
    let drive =
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\');
    if v.is_empty() {
        Some("cannot be empty")
    } else if v.contains('\0') {
        Some("cannot contain NUL bytes")
    } else if v.starts_with(['/', '\\']) || drive {
        Some("must be relative")
    } else if path_parts(v).any(|p| p == "..") {
        Some("cannot contain parent-directory components")
    } else {
        None
    }
}

/// Whether a workspace's documents would sit under a folder whose name
/// starts with `role:` or `run:` (ASCII, any case): the first segment of
/// its path, then its `mdkg_dir`. Node scopes there would overlap a role's
/// or a turn's lease scope, so such a workspace is refused.
fn under_a_lease_name(path: &str, mdkg_dir: &str) -> bool {
    path_parts(path)
        .chain(path_parts(mdkg_dir))
        .find(|p| *p != ".")
        .is_some_and(|p| {
            let p = p.to_ascii_lowercase();
            p.starts_with("role:") || p.starts_with("run:")
        })
}

/// A whole number written as JavaScript writes it (`String(n)`: exponent
/// form from 1e21). Only the number's form matches 0.6.0, not the message,
/// which names a different supported version. serde_json here isn't built
/// with `float_roundtrip`, so a long literal may parse to a neighbouring
/// double first, a known difference from JavaScript's number parsing.
fn js_number(n: f64) -> String {
    if n.abs() < 1e21 {
        format!("{n}")
    } else {
        format!("{n:e}").replacen('e', "e+", 1)
    }
}

/// Keeps the template folder in the repository (audit, 2026-10-22): an
/// absolute `templates.root_path` or `default_set`, a `..` in one, or a
/// link on the way out of the repository is an error, like a workspace
/// path's.
fn templates_inside(root: &Path, v: &Value, errors: &mut Vec<String>) {
    let mut folder = root.to_path_buf();
    let mut bad = false;
    for key in ["root_path", "default_set"] {
        match v.pointer(&format!("/templates/{key}")) {
            None => {}
            Some(Value::String(value)) => match contained_path_error(value) {
                Some(e) => {
                    errors.push(format!("templates.{key} {e}"));
                    bad = true;
                }
                None => folder.push(mdkg_format::js::trim(value)),
            },
            // Not a string: the default is used, as before.
            Some(_) => {}
        }
    }
    if bad {
        return;
    }
    // Only a folder that exists can lead anywhere.
    if let (Ok(real), Ok(real_root)) = (std::fs::canonicalize(&folder), std::fs::canonicalize(root))
        && !real.starts_with(&real_root)
    {
        errors.push(
            "templates.root_path must stay inside the repository; a link on the way leads out of it"
                .to_string(),
        );
    }
}

/// Whether a workspace path names the repository root (`.`, `./.`).
fn is_root_path(value: &str) -> bool {
    let mut parts = path_parts(value).peekable();
    parts.peek().is_some() && parts.all(|p| p == ".")
}

/// Whether `ws` is the graph's root workspace (its path is the graph
/// root), whose work nodes profile C keeps beside the store.
pub fn is_root_workspace(ws: &Workspace) -> bool {
    is_root_path(&ws.path)
}

/// The document root a workspace reads, with `.` parts dropped, so `x` and
/// `./x` are the same root.
fn document_root_key(path: &str, mdkg_dir: &str) -> String {
    path_parts(path)
        .chain(path_parts(mdkg_dir))
        .filter(|p| *p != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// The format and `graph_id` a config declares (format 0.7, section 2):
/// `schema_version` 3 needs `format_version` "0.7" and a UUID `graph_id`
/// (init mints a v7; a migrated identity v2 graph keeps its own), and older
/// configs may have neither.
fn read_identity(v: &Value) -> Result<(FormatVersion, Option<Uuid7>), GraphError> {
    let fail = |message: &str| {
        Err(GraphError::failure(Code::ConfigInvalid, message.to_string()).at(CONFIG_PATH, None))
    };
    let v07 = v.get("schema_version").and_then(Value::as_f64) == Some(V07_SCHEMA_VERSION as f64);
    let (format_version, graph_id) = (v.get("format_version"), v.get("graph_id"));
    if !v07 {
        if format_version.is_some() || graph_id.is_some() {
            return fail("config graph_id and format_version need schema_version 3");
        }
        return Ok((FormatVersion::Legacy, None));
    }
    if format_version.and_then(Value::as_str) != Some(FORMAT_VERSION_07) {
        return fail("config format_version must be \"0.7\" with schema_version 3");
    }
    match graph_id.and_then(Value::as_str).map(Uuid7::parse) {
        Some(Ok(id)) => Ok((FormatVersion::V07, Some(id))),
        _ => fail("config graph_id must be a lowercase UUID with schema_version 3"),
    }
}

/// Whether `root` holds an mdkg config.
pub fn has_config(root: &Path) -> bool {
    root.join(CONFIG_PATH).is_file()
}

/// Loads and checks the config at `root/.mdkg/config.json`. A config with
/// `schema_version` 2 (0.6.0's identity format v2) is refused, with a hint
/// to migrate it.
pub fn load(root: &Path) -> Result<Config, GraphError> {
    load_with(root, false)
}

/// [`load`] for `migrate` reading 0.6.0's identity format v2
/// ([`crate::identity_v2`]): `schema_version` 2 reads as a legacy config.
pub fn load_identity_v2(root: &Path) -> Result<Config, GraphError> {
    load_with(root, true)
}

/// Refuses a config this build can't read, before a command does
/// anything: a `schema_version` that isn't a non-negative integer, is
/// newer than this build reads, or is 2 (read only by `migrate`); and a
/// version 3 config whose `format_version` isn't "0.7" or whose
/// `graph_id` isn't a UUID. The CLI checks it once for every command that
/// reads a graph, since a released binary can't learn the refusal later;
/// [`load`] makes the same checks.
pub fn check_version(root: &Path) -> Result<(), GraphError> {
    check_version_with(root, false)
}

/// [`check_version`] for `migrate`, which reads 0.6.0's identity format v2
/// (`schema_version` 2) too, and refuses only what's newer than this build
/// or invalid, before it writes its lock.
pub fn check_version_for_migrate(root: &Path) -> Result<(), GraphError> {
    check_version_with(root, true)
}

fn check_version_with(root: &Path, identity_v2: bool) -> Result<(), GraphError> {
    let v = read_json(root)?;
    if !v.is_object() {
        return Err(
            GraphError::failure(Code::ConfigInvalid, "config must be a JSON object")
                .at(CONFIG_PATH, None),
        );
    }
    check_schema_version(&v, identity_v2)?;
    read_identity(&v).map(|_| ())
}

/// The config's JSON, as read from `root/.mdkg/config.json`.
fn read_json(root: &Path) -> Result<Value, GraphError> {
    let path = root.join(CONFIG_PATH);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        GraphError::failure(
            Code::ConfigUnreadable,
            format!("failed to read config: {e}"),
        )
        .at(CONFIG_PATH, None)
    })?;
    serde_json::from_str(&text).map_err(|e| {
        GraphError::failure(
            Code::ConfigUnreadable,
            format!("config is not valid JSON: {e}"),
        )
        .at(CONFIG_PATH, None)
    })
}

/// Refuses a `schema_version` this build doesn't read; `identity_v2`
/// reads 2 (0.6.0's identity format v2) for `migrate`.
fn check_schema_version(v: &Value, identity_v2: bool) -> Result<(), GraphError> {
    // As 0.6.0 reads it: absent means legacy, otherwise a non-negative
    // integer, where JSON's 0.0 and -0 are the integer 0.
    let Some(version) = v.get("schema_version") else {
        return Ok(());
    };
    let message: Option<String> = match version.as_f64() {
        // serde_json refuses out-of-range numbers such as 1e400 as
        // unreadable JSON, so `n` is always finite here.
        Some(n) if n.fract() != 0.0 => Some("config schema_version must be an integer".into()),
        Some(n) if n < 0.0 => Some("config schema_version must be non-negative".into()),
        // A newer config may hold rules this build would ignore, so it
        // isn't read. 0.6.0 reads up to 2 and refuses newer ones the
        // same way; mdkg reads 1 and 3 (a recorded divergence).
        Some(2.0) if identity_v2 => None,
        Some(2.0) => {
            Some("config schema_version 2 (identity format v2) is read only by migrate".into())
        }
        Some(n) if n > MAX_SCHEMA_VERSION as f64 => Some(format!(
            "config schema_version {} is newer than supported {MAX_SCHEMA_VERSION}",
            js_number(n)
        )),
        Some(_) => None,
        None => Some("config schema_version must be an integer".into()),
    };
    let Some(message) = message else {
        return Ok(());
    };
    let mut e = GraphError::failure(Code::ConfigInvalid, message).at(CONFIG_PATH, None);
    // Version 2 is what 0.6.0's identity migration writes.
    if version.as_f64() == Some(2.0) {
        e = e.with_hint(identity_v2_hint());
    }
    Err(e)
}

fn load_with(root: &Path, identity_v2: bool) -> Result<Config, GraphError> {
    let v = read_json(root)?;
    from_value(root, v, identity_v2, || crate::store_work::work_dir(root))
}

/// Parse a captured value. The work directory belongs to the same snapshot as
/// that value; physical-only callers retain their existing discovery behavior.
fn from_value(
    root: &Path,
    v: Value,
    identity_v2: bool,
    work_dir: impl FnOnce() -> Result<Option<std::path::PathBuf>, String>,
) -> Result<Config, GraphError> {
    let mut errors = Vec::new();
    let mut workspaces = Vec::new();
    // Which workspace owns each document root, as 0.6.0 checks it.
    let mut owners: Vec<(String, String)> = Vec::new();
    // A legacy config (no `schema_version`, or 0) without workspaces gets
    // the root workspace, as 0.6.0's migration gives it.
    if !v.is_object() {
        return Err(
            GraphError::failure(Code::ConfigInvalid, "config must be a JSON object")
                .at(CONFIG_PATH, None),
        );
    }
    check_schema_version(&v, identity_v2)?;
    let (format, graph_id) = read_identity(&v)?;
    // A legacy config, and a format 0.7 one (which init writes with no
    // workspaces key), get the root workspace when they name none.
    let implied_root = v
        .get("schema_version")
        .is_none_or(|s| s.as_f64() == Some(0.0))
        || format == FormatVersion::V07;
    let legacy_root = serde_json::json!({
        "root": { "path": ".", "enabled": true, "mdkg_dir": ".mdkg" }
    });
    let declared = match v.get("workspaces") {
        None if implied_root => Some(&legacy_root),
        other => other,
    };
    if let Some(map) = declared.and_then(Value::as_object) {
        for (alias, entry) in map {
            if !is_alias(alias) {
                errors.push(format!(
                    "workspaces.{alias} alias must be lowercase and use [a-z0-9_]"
                ));
                continue;
            }
            if !entry.is_object() {
                errors.push(format!("workspaces.{alias} must be an object"));
                continue;
            }
            // A wrong-typed value is an error, with 0.6.0's message. A
            // missing `mdkg_dir` or `enabled`, and the root's missing
            // `path`, take their defaults, where 0.6.0 requires them. Only
            // the root has a default `path`: a non-root workspace without
            // `path` is an error.
            let text = |key: &str, default: Option<&'static str>| match entry.get(key) {
                None => default.ok_or(()),
                Some(Value::String(s)) => Ok(mdkg_format::js::trim(s)),
                Some(_) => Err(()),
            };
            let path = text("path", (alias == "root").then_some("."));
            let mdkg_dir = text("mdkg_dir", Some(".mdkg"));
            let enabled = match entry.get("enabled") {
                None => Ok(true),
                Some(Value::Bool(b)) => Ok(*b),
                Some(_) => Err(()),
            };
            let (Ok(path), Ok(mdkg_dir), Ok(enabled)) = (path, mdkg_dir, enabled) else {
                for (key, value) in [("path", path), ("mdkg_dir", mdkg_dir)] {
                    if value.is_err() {
                        errors.push(format!("workspaces.{alias}.{key} must be a string"));
                    }
                }
                if enabled.is_err() {
                    errors.push(format!("workspaces.{alias}.enabled must be a boolean"));
                }
                continue;
            };
            let bad: Vec<String> = [("path", path), ("mdkg_dir", mdkg_dir)]
                .into_iter()
                .filter_map(|(key, value)| {
                    contained_path_error(value).map(|e| format!("workspaces.{alias}.{key} {e}"))
                })
                .collect();
            if !bad.is_empty() {
                errors.extend(bad);
                continue;
            }
            if alias != "root" && is_root_path(path) {
                errors.push(format!(
                    "workspaces.{alias}.path must not be \".\" for non-root workspaces"
                ));
                continue;
            }
            // Root's fields are fixed, and get their own errors below.
            if alias != "root" && under_a_lease_name(path, mdkg_dir) {
                errors.push(format!(
                    "workspaces.{alias}.path must not start with a `role:` or `run:` folder, which names a role's or a turn's lease"
                ));
                continue;
            }
            let key = document_root_key(path, mdkg_dir);
            if let Some((_, owner)) = owners.iter().find(|(k, _)| *k == key) {
                errors.push(format!(
                    "workspaces.{alias} document root duplicates workspaces.{owner}"
                ));
                continue;
            }
            owners.push((key, alias.clone()));
            workspaces.push(Workspace {
                alias: alias.clone(),
                path: path.to_string(),
                enabled,
                mdkg_dir: mdkg_dir.to_string(),
            });
        }
    }
    // The root workspace is required and fixed (0.6.0).
    match workspaces.iter().find(|w| w.alias == "root") {
        None => errors.push("workspaces.root is required".to_string()),
        Some(root) => {
            if root.path != "." {
                errors.push("workspaces.root.path must be \".\"".to_string());
            }
            if !root.enabled {
                errors.push("workspaces.root.enabled must be true".to_string());
            }
            if root.mdkg_dir != ".mdkg" {
                errors.push("workspaces.root.mdkg_dir must be \".mdkg\"".to_string());
            }
        }
    }
    let leases = lease_policy(&v, &mut errors);
    templates_inside(root, &v, &mut errors);
    if !errors.is_empty() {
        return Err(GraphError::failure(
            Code::ConfigInvalid,
            format!("config validation failed: {}", errors.join("; ")),
        )
        .at(CONFIG_PATH, None));
    }
    workspaces.sort_by(|a, b| a.alias.encode_utf16().cmp(b.alias.encode_utf16()));
    // Opt-in projections: a config that lists them mirrors only the skill
    // folders it lists (none when it lists none).
    let projected = v.get("projections").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .filter_map(|s| s.strip_prefix("skills:").map(String::from))
            .collect::<Vec<String>>()
    });
    let skill_mirror_targets = projected.unwrap_or_else(|| {
        v.pointer("/customization/skill_mirrors/targets")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec![".agents/skills".to_string(), ".claude/skills".to_string()])
    });

    let status_enum = v
        .pointer("/work/status_enum")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| {
            ["backlog", "blocked", "todo", "progress", "review", "done"]
                .map(String::from)
                .to_vec()
        });
    let subgraphs = v
        .get("subgraphs")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    let location = match v.pointer("/work/location") {
        None => None,
        Some(Value::String(s)) if s == "git" => Some(WorkCommitted::Definitions),
        Some(Value::String(s)) if s == "local" && format == FormatVersion::V07 => {
            Some(WorkCommitted::None)
        }
        Some(Value::String(s)) if s == "local" => {
            return Err(GraphError::failure(
                Code::ConfigInvalid,
                "config work.location \"local\" needs a format 0.7 graph (schema_version 3)",
            )
            .at(CONFIG_PATH, None));
        }
        Some(other) => {
            return Err(GraphError::failure(
                Code::ConfigInvalid,
                format!("config work.location must be \"local\" or \"git\", not {other}"),
            )
            .at(CONFIG_PATH, None));
        }
    };
    let alias = match v.pointer("/work/committed") {
        None => None,
        Some(Value::String(s)) if s == "definitions" => Some(WorkCommitted::Definitions),
        Some(Value::String(s)) if s == "none" && format == FormatVersion::V07 => {
            Some(WorkCommitted::None)
        }
        Some(Value::String(s)) if s == "none" => {
            return Err(GraphError::failure(
                Code::ConfigInvalid,
                "config work.committed \"none\" needs a format 0.7 graph (schema_version 3)",
            )
            .at(CONFIG_PATH, None));
        }
        Some(other) => {
            return Err(GraphError::failure(
                Code::ConfigInvalid,
                format!("config work.committed must be \"definitions\" or \"none\", not {other}"),
            )
            .at(CONFIG_PATH, None));
        }
    };
    if location.zip(alias).is_some_and(|(a, b)| a != b) {
        return Err(GraphError::failure(
            Code::ConfigInvalid,
            "config work.location conflicts with its work.committed alias",
        )
        .at(CONFIG_PATH, None));
    }
    // Existing graphs that omitted the choice must not silently stop reading
    // their committed nodes. New init writes the local choice explicitly.
    let work_committed = location.or(alias).unwrap_or_default();
    Ok(Config {
        tool: str_at(&v, &["tool"]).unwrap_or("mdkg").to_string(),
        // A legacy config migrates to 1, as in 0.6.0.
        schema_version: v
            .get("schema_version")
            .and_then(Value::as_f64)
            .filter(|n| *n != 0.0)
            .map_or(1, |n| n as i64),
        format,
        graph_id,
        status_enum,
        next_status_preference: v
            .pointer("/work/next/status_preference")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_lowercase)
                    .collect()
            })
            .unwrap_or_else(|| {
                ["progress", "todo", "review", "blocked", "backlog"]
                    .map(String::from)
                    .to_vec()
            }),
        priority_min: int_at(&v, &["work", "priority_min"]).unwrap_or(0),
        priority_max: int_at(&v, &["work", "priority_max"]).unwrap_or(9),
        workspaces,
        skill_mirror_targets,
        templates_root: str_at(&v, &["templates", "root_path"])
            .unwrap_or(".mdkg/templates")
            .to_string(),
        templates_set: str_at(&v, &["templates", "default_set"])
            .unwrap_or("default")
            .to_string(),
        limits: Limits {
            max_files: int_at(&v, &["index", "limits", "max_files"]).unwrap_or(100_000) as u64,
            max_file_bytes: int_at(&v, &["index", "limits", "max_file_bytes"])
                .unwrap_or(8 * 1024 * 1024) as u64,
            max_total_bytes: int_at(&v, &["index", "limits", "max_total_bytes"])
                .unwrap_or(512 * 1024 * 1024) as u64,
            max_depth: int_at(&v, &["index", "limits", "max_depth"]).unwrap_or(64) as u64,
        },
        tolerant: bool_at(&v, &["index", "tolerant"]).unwrap_or(false),
        subgraphs,
        leases,
        work_committed,
        store_work_dir: match work_committed {
            WorkCommitted::None => work_dir().map_err(|e| {
                GraphError::failure(
                    Code::IoReadFailed,
                    format!("can't find the folder beside the coordinator store: {e}"),
                )
                .with_hint(
                    "work.location \"local\" keeps work nodes in the repository's git folder",
                )
            })?,
            WorkCommitted::Definitions => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_with(workspaces: serde_json::Value) -> Result<Config, GraphError> {
        load_config(serde_json::json!({ "workspaces": workspaces }))
    }

    fn load_config(config: serde_json::Value) -> Result<Config, GraphError> {
        load_text(&config.to_string())
    }

    /// Loads `text` as a repository's config, in a folder of its own.
    fn load_text(text: &str) -> Result<Config, GraphError> {
        // The clock alone can repeat across parallel tests.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "mdkg-config-{}-{n}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(dir.join(".mdkg")).expect("mkdir");
        std::fs::write(dir.join(CONFIG_PATH), text).expect("write");
        let loaded = load(&dir);
        std::fs::remove_dir_all(&dir).ok();
        loaded
    }

    fn message(r: Result<Config, GraphError>) -> String {
        r.expect_err("config should be rejected").diagnostic.message
    }

    fn work_config(work: Value) -> Value {
        serde_json::json!({"schema_version":3, "format_version":"0.7",
            "graph_id":"0192f3a0-0000-7000-8000-000000000000", "work":work})
    }

    #[test]
    fn work_location_and_legacy_alias_select_the_same_storage() {
        for (location, alias, expected) in [
            ("local", "none", WorkCommitted::None),
            ("git", "definitions", WorkCommitted::Definitions),
        ] {
            for work in [
                serde_json::json!({"location":location}),
                serde_json::json!({"committed":alias}),
                serde_json::json!({"location":location,"committed":alias}),
            ] {
                let cfg = load_config(work_config(work)).unwrap();
                assert_eq!(cfg.work_committed, expected);
                assert_eq!(cfg.work_committed.location(), location);
            }
        }
        assert_eq!(
            load_config(work_config(serde_json::json!({})))
                .unwrap()
                .work_committed,
            WorkCommitted::Definitions,
            "old implicit storage must keep reading committed work"
        );
    }

    #[test]
    fn work_location_rejects_unknown_values_and_conflicting_aliases() {
        for value in [
            Value::Null,
            serde_json::json!(true),
            serde_json::json!(1),
            serde_json::json!("shared"),
            serde_json::json!("none"),
        ] {
            assert!(
                message(load_config(work_config(
                    serde_json::json!({"location":value})
                )))
                .contains("work.location")
            );
        }
        for work in [
            serde_json::json!({"location":"local","committed":"definitions"}),
            serde_json::json!({"location":"git","committed":"none"}),
        ] {
            assert!(message(load_config(work_config(work))).contains("conflicts"));
        }
        assert!(
            message(load_config(work_config(
                serde_json::json!({"location":"local","committed":"wrong"})
            )))
            .contains("work.committed")
        );
        assert!(
            message(load_config(
                serde_json::json!({"schema_version":1,"work":{"location":"local"},"workspaces":{"root":{}}})
            ))
            .contains("needs a format 0.7 graph")
        );
    }

    #[test]
    fn format_0_7_needs_its_identity_keys() {
        let graph = "0192f3a0-0000-7000-8000-000000000000";
        let ws = serde_json::json!({ "root": {} });
        let ok = load_config(serde_json::json!({
            "schema_version": 3, "format_version": "0.7", "graph_id": graph, "workspaces": ws
        }))
        .expect("loads");
        assert_eq!(ok.format, FormatVersion::V07);
        assert_eq!(ok.graph_id.map(|g| g.to_string()).as_deref(), Some(graph));
        // init's minimal config names no workspaces: the root is implied.
        let minimal = load_config(serde_json::json!({
            "schema_version": 3, "format_version": "0.7", "graph_id": graph
        }))
        .expect("loads");
        assert_eq!(minimal.workspaces.len(), 1);
        assert_eq!(minimal.workspaces[0].alias, "root");
        assert_eq!(minimal.workspaces[0].path, ".");
        let legacy = load_config(serde_json::json!({ "schema_version": 1, "workspaces": ws }))
            .expect("loads");
        assert_eq!(
            (legacy.format, legacy.graph_id),
            (FormatVersion::Legacy, None)
        );
        for (config, expected) in [
            (
                serde_json::json!({ "schema_version": 3, "graph_id": graph, "workspaces": ws }),
                "config format_version must be \"0.7\" with schema_version 3",
            ),
            (
                serde_json::json!({ "schema_version": 3, "format_version": "0.8", "graph_id": graph, "workspaces": ws }),
                "config format_version must be \"0.7\" with schema_version 3",
            ),
            (
                serde_json::json!({ "schema_version": 3, "format_version": "0.7", "workspaces": ws }),
                "config graph_id must be a lowercase UUID with schema_version 3",
            ),
            (
                // No RFC 9562 version.
                serde_json::json!({ "schema_version": 3, "format_version": "0.7", "graph_id": "0192f3a0-0000-0000-8000-000000000000", "workspaces": ws }),
                "config graph_id must be a lowercase UUID with schema_version 3",
            ),
            (
                serde_json::json!({ "schema_version": 1, "graph_id": graph, "workspaces": ws }),
                "config graph_id and format_version need schema_version 3",
            ),
            (
                serde_json::json!({ "format_version": "0.7" }),
                "config graph_id and format_version need schema_version 3",
            ),
        ] {
            assert_eq!(message(load_config(config)), expected);
        }
    }

    #[test]
    fn a_templates_root_outside_the_repository_is_an_error() {
        // audit, 2026-10-22: templates were read from wherever it pointed.
        for (templates, expected) in [
            (
                serde_json::json!({ "root_path": "/etc" }),
                "templates.root_path must be relative",
            ),
            (
                serde_json::json!({ "root_path": "../elsewhere" }),
                "templates.root_path cannot contain parent-directory components",
            ),
            (
                serde_json::json!({ "root_path": ".mdkg/templates", "default_set": "../../x" }),
                "templates.default_set cannot contain parent-directory components",
            ),
        ] {
            let m = message(load_config(serde_json::json!({ "templates": templates })));
            assert!(m.contains(expected), "{m}");
        }
        let ok = load_config(serde_json::json!({ "templates": { "root_path": "./t" } }));
        assert_eq!(ok.expect("inside").templates_root, "./t");
    }

    #[cfg(unix)]
    #[test]
    fn a_templates_root_linked_out_of_the_repository_is_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-config-link-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let (repo, outside) = (dir.join("repo"), dir.join("outside"));
        std::fs::create_dir_all(repo.join(".mdkg")).expect("mkdir");
        std::fs::create_dir_all(outside.join("default")).expect("mkdir");
        std::fs::create_dir_all(repo.join("inner/default")).expect("mkdir");
        let config = |root: &str| serde_json::json!({ "templates": { "root_path": root } });
        std::os::unix::fs::symlink(&outside, repo.join("t")).expect("ln");
        std::os::unix::fs::symlink("inner", repo.join("u")).expect("ln");
        std::fs::write(repo.join(CONFIG_PATH), config("t").to_string()).expect("w");
        let m = message(load(&repo));
        assert!(
            m.contains("templates.root_path must stay inside the repository"),
            "{m}"
        );
        // A link that stays inside is fine.
        std::fs::write(repo.join(CONFIG_PATH), config("u").to_string()).expect("w");
        assert!(load(&repo).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn workspace_paths_are_checked_as_0_6_0_checks_them() {
        let root = serde_json::json!({ "path": ".", "mdkg_dir": ".mdkg" });
        assert!(load_with(serde_json::json!({ "root": root })).is_ok());
        let ok = load_with(serde_json::json!({ "root": root, "ws1": { "path": "x" } }));
        assert_eq!(ok.expect("valid").workspaces.len(), 2);
        for (ws1, expected) in [
            (
                serde_json::json!({ "path": "." }),
                "workspaces.ws1.path must not be \".\" for non-root workspaces",
            ),
            (
                serde_json::json!({ "path": "./.", "mdkg_dir": "other" }),
                "must not be \".\"",
            ),
            (
                serde_json::json!({ "path": "/abs" }),
                "workspaces.ws1.path must be relative",
            ),
            (serde_json::json!({ "path": "C:\\x" }), "must be relative"),
            (
                serde_json::json!({ "path": "a/../b" }),
                "workspaces.ws1.path cannot contain parent-directory components",
            ),
            (
                serde_json::json!({ "path": " " }),
                "workspaces.ws1.path cannot be empty",
            ),
            (
                serde_json::json!({ "path": "x", "mdkg_dir": "../m" }),
                "workspaces.ws1.mdkg_dir cannot contain",
            ),
        ] {
            let m = message(load_with(serde_json::json!({ "root": root, "ws1": ws1 })));
            assert!(m.contains(expected), "{m}");
        }
        let m = message(load_with(serde_json::json!({
            "root": root,
            "ws1": { "path": "x" },
            "ws2": { "path": "./x/" },
        })));
        assert!(
            m.contains("workspaces.ws2 document root duplicates workspaces.ws1"),
            "{m}"
        );
    }

    #[test]
    fn workspaces_under_role_or_run_folders_are_refused() {
        let root = serde_json::json!({ "path": ".", "mdkg_dir": ".mdkg" });
        for ws1 in [
            serde_json::json!({ "path": "role:x" }),
            serde_json::json!({ "path": "run:lane-a/sub" }),
            serde_json::json!({ "path": "./ROLE:x" }),
            serde_json::json!({ "path": " Run:x " }),
        ] {
            let m = message(load_with(serde_json::json!({ "root": root, "ws1": ws1 })));
            assert!(
                m.contains("workspaces.ws1.path must not start with"),
                "{ws1}: {m}"
            );
            assert!(!m.contains("lane-a") && !m.contains(":x"), "no echo: {m}");
        }
        // Root keeps its own, more precise errors.
        for (root, expected) in [
            (
                serde_json::json!({ "path": ".", "mdkg_dir": "role:x" }),
                "workspaces.root.mdkg_dir must be \".mdkg\"",
            ),
            (
                serde_json::json!({ "path": "./run:x" }),
                "workspaces.root.path must be \".\"",
            ),
        ] {
            let m = message(load_with(serde_json::json!({ "root": root })));
            assert!(m.contains(expected), "{m}");
            assert!(
                !m.contains("must not start with") && !m.contains("is required"),
                "{m}"
            );
        }
        // Only the first segment names a lease; a name that merely
        // contains one is fine.
        for path in ["x/role:y", "roles", "runner", "my-role:x"] {
            let ws1 = serde_json::json!({ "path": path });
            assert!(
                load_with(serde_json::json!({ "root": root, "ws1": ws1 })).is_ok(),
                "{path}"
            );
        }
    }

    #[test]
    fn workspace_paths_are_stored_trimmed() {
        let root = serde_json::json!({ "path": ".", "mdkg_dir": ".mdkg" });
        let config = load_with(serde_json::json!({
            "root": root,
            "ws1": { "path": " x ", "mdkg_dir": " .mdkg" },
        }))
        .expect("valid");
        let ws1 = config
            .workspaces
            .iter()
            .find(|w| w.alias == "ws1")
            .expect("ws1");
        assert_eq!((ws1.path.as_str(), ws1.mdkg_dir.as_str()), ("x", ".mdkg"));
    }

    #[test]
    fn workspace_entries_need_string_paths_and_a_fixed_root() {
        let root = serde_json::json!({ "path": ".", "mdkg_dir": ".mdkg" });
        for (ws1, expected) in [
            (
                serde_json::json!({ "enabled": true }),
                "workspaces.ws1.path must be a string",
            ),
            (
                serde_json::json!({ "path": 5 }),
                "workspaces.ws1.path must be a string",
            ),
            (
                serde_json::json!({ "path": "x", "mdkg_dir": [] }),
                "workspaces.ws1.mdkg_dir must be a string",
            ),
            (serde_json::json!("x"), "workspaces.ws1 must be an object"),
            (
                serde_json::json!({ "path": "x", "enabled": "no" }),
                "workspaces.ws1.enabled must be a boolean",
            ),
        ] {
            let m = message(load_with(serde_json::json!({ "root": root, "ws1": ws1 })));
            assert!(m.contains(expected), "{m}");
            assert!(!m.contains("must not be"), "{m}");
        }
        // The root may leave out `path` and `mdkg_dir`.
        assert!(load_with(serde_json::json!({ "root": {} })).is_ok());
        for (workspaces, expected) in [
            (serde_json::json!({}), "workspaces.root is required"),
            (
                serde_json::json!({ "root": { "path": "./" } }),
                "workspaces.root.path must be \".\"",
            ),
            (
                serde_json::json!({ "root": { "enabled": false } }),
                "workspaces.root.enabled must be true",
            ),
            (
                serde_json::json!({ "root": { "mdkg_dir": "m" } }),
                "workspaces.root.mdkg_dir must be \".mdkg\"",
            ),
        ] {
            let m = message(load_with(workspaces));
            assert!(m.contains(expected), "{m}");
        }
    }

    #[test]
    fn a_legacy_config_without_workspaces_gets_the_root() {
        for legacy in [
            serde_json::json!({}),
            serde_json::json!({ "schema_version": 0 }),
            serde_json::json!({ "schema_version": 0.0 }),
            serde_json::json!({ "schema_version": -0.0 }),
        ] {
            let config = load_config(legacy).expect("legacy config loads");
            assert_eq!(config.schema_version, 1);
            assert_eq!(config.workspaces.len(), 1);
            assert_eq!(config.workspaces[0].alias, "root");
        }
        let one =
            load_config(serde_json::json!({ "schema_version": 1.0, "workspaces": { "root": {} } }));
        assert_eq!(one.expect("loads").schema_version, 1);
        let m = message(load_config(serde_json::json!({ "schema_version": 1 })));
        assert!(m.contains("workspaces.root is required"), "{m}");
        for (version, expected) in [
            (
                serde_json::json!("1"),
                "config schema_version must be an integer",
            ),
            (
                serde_json::json!(0.5),
                "config schema_version must be an integer",
            ),
            (
                serde_json::json!(null),
                "config schema_version must be an integer",
            ),
            (
                serde_json::json!(-1),
                "config schema_version must be non-negative",
            ),
            (
                serde_json::json!(2),
                "config schema_version 2 (identity format v2) is read only by migrate",
            ),
            (
                serde_json::json!(1e20),
                "config schema_version 100000000000000000000 is newer than supported 3",
            ),
            (
                serde_json::json!(1e21),
                "config schema_version 1e+21 is newer than supported 3",
            ),
        ] {
            let m = message(load_config(
                serde_json::json!({ "schema_version": version }),
            ));
            // The whole message: these are refused before anything else.
            assert_eq!(m, expected);
        }
        let newer =
            load_config(serde_json::json!({ "schema_version": 2, "workspaces": { "root": {} } }))
                .expect_err("refused");
        assert_eq!(newer.code(), Code::ConfigInvalid);
        assert_eq!(newer.diagnostic.path.as_deref(), Some(CONFIG_PATH));
        // Version 2, however it's spelled, says where it came from and how
        // to get back; 3 never comes from 0.6.0, so it has no such hint.
        for two in ["2", "2.0", "2e0"] {
            let e = load_text(&format!(
                r#"{{"schema_version": {two}, "workspaces": {{"root": {{}}}}}}"#
            ))
            .expect_err("refused");
            assert_eq!(
                e.diagnostic.hint.as_deref(),
                Some(identity_v2_hint().as_str()),
                "{two}"
            );
        }
        // The whole hint, so nothing can bring back a hand restore:
        // the source, migrating forward, 0.6.0's rollback.
        assert_eq!(
            identity_v2_hint(),
            "this graph is in identity format v2, written by mdkg 0.6.0 (`mdkg graph migrate --apply`, or a v2 `graph clone`/`fork`); run `mdkg migrate --dry-run`, then `--apply`, to bring it to format 0.7, keeping its graph and node UUIDs. If nothing has changed since the identity migration, 0.6.0's `mdkg graph recover` can still roll it back in two steps"
        );
        let three =
            load_config(serde_json::json!({ "schema_version": 3, "workspaces": { "root": {} } }))
                .expect_err("refused");
        assert!(
            three.diagnostic.hint.is_none(),
            "only version 2 has that hint"
        );
        let four =
            load_config(serde_json::json!({ "schema_version": 4, "workspaces": { "root": {} } }));
        assert_eq!(
            message(four),
            "config schema_version 4 is newer than supported 3"
        );
        // Pins the difference from JavaScript's number parsing: if serde_json
        // uses `float_roundtrip`, review these compatibility expectations.
        let m = message(load_text(r#"{"schema_version": 1.00000000000000011}"#));
        assert_eq!(m, "config schema_version must be an integer");
        // Fails open: 0.6.0 refuses these as not integers; here they read
        // as version 1 and as legacy 0.
        // One literal for both checks, so they can't drift apart. JavaScript
        // writes 1-2^-53 as 0.9999999999999999, which reads as 1 here too.
        for near_one in ["0.99999999999999994", "0.9999999999999999"] {
            let one = load_text(&format!(
                r#"{{"schema_version": {near_one}, "workspaces": {{"root": {{}}}}}}"#
            ))
            .expect("serde_json rounds the fractional schema version to 1");
            assert_eq!(one.schema_version, 1);
            // As exactly 1, not as legacy 0: without workspaces, 1 needs a root.
            let m = message(load_text(&format!(r#"{{"schema_version": {near_one}}}"#)));
            assert_eq!(m, "config validation failed: workspaces.root is required");
        }
        let legacy = load_text(r#"{"schema_version": 2.4703282292062328e-324}"#)
            .expect("serde_json rounds the tiny positive schema version to legacy 0");
        assert_eq!(legacy.workspaces[0].alias, "root");
        let m = message(load_text(r#"{"schema_version": 563347472885448035408}"#));
        assert_eq!(
            m,
            "config schema_version 563347472885448000000 is newer than supported 3"
        );
        for not_an_object in [
            serde_json::json!([]),
            serde_json::json!(null),
            serde_json::json!(5),
        ] {
            let m = message(load_config(not_an_object));
            assert!(m.contains("config must be a JSON object"), "{m}");
        }
    }

    #[test]
    fn an_out_of_range_number_makes_the_config_unreadable() {
        // serde_json refuses 1e400, so `schema_version` is never infinite.
        let e = load_text(r#"{"schema_version": 1e400}"#).expect_err("refused");
        assert_eq!(e.code(), Code::ConfigUnreadable);
        // The cause, not just any parse error: a typo in the literal mustn't
        // pass for it.
        assert!(
            e.diagnostic.message.contains("number out of range"),
            "{}",
            e.diagnostic.message
        );
    }

    fn policy(default: LeaseMode, paths: &[(&str, LeaseMode)]) -> LeasePolicy {
        LeasePolicy {
            default,
            paths: paths.iter().map(|(g, m)| (g.to_string(), *m)).collect(),
            unleased: Unleased::Warn,
        }
    }

    use LeaseMode::{Exclusive as X, Shared as S};

    fn g() -> Vec<String> {
        vec![".mdkg".to_string()]
    }

    #[test]
    fn globs_match_whole_segments() {
        let p = policy(
            X,
            &[
                ("go.sum", S),
                ("**/*_test.go", S),
                ("crates/*/src/main.rs", S),
            ],
        );
        assert_eq!(p.mode_for("go.sum", &g(), false), S);
        assert_eq!(
            p.mode_for("sub/go.sum", &g(), false),
            X,
            "anchored at the root"
        );
        assert_eq!(p.mode_for("a/b/x_test.go", &g(), false), S);
        assert_eq!(
            p.mode_for("x_test.go", &g(), false),
            S,
            "** spans zero segments"
        );
        assert_eq!(p.mode_for("crates/cli/src/main.rs", &g(), false), S);
        assert_eq!(p.mode_for("crates/a/b/src/main.rs", &g(), false), X);
        assert_eq!(p.mode_for("GO.SUM", &g(), false), S, "case never matters");
    }

    #[test]
    fn the_most_specific_match_wins_then_the_earliest() {
        let p = policy(X, &[("src/**", S), ("src/secret.rs", X), ("src/*.rs", X)]);
        assert_eq!(p.mode_for("src/secret.rs", &g(), false), X);
        assert_eq!(
            p.mode_for("src/lib.rs", &g(), false),
            X,
            "src/*.rs beats src/**"
        );
        assert_eq!(p.mode_for("src/a/b.rs", &g(), false), S);
        let tie = policy(X, &[("src/*", S), ("src/?.rs", X)]);
        assert_eq!(tie.mode_for("src/a.rs", &g(), false), S);
    }

    #[test]
    fn nodes_are_always_exclusive() {
        let p = policy(S, &[("**", S)]);
        assert_eq!(p.mode_for(".mdkg/work/task-1.md", &g(), false), X);
        assert_eq!(p.mode_for(".MDKG/work", &g(), true), X);
        assert_eq!(
            p.mode_for("", &g(), true),
            X,
            "the whole tree holds the graph"
        );
        assert_eq!(p.mode_for("src/a.rs", &g(), false), S);
        assert_eq!(p.mode_for(".mdkgx/a", &g(), false), S);
        let ws = vec![".mdkg".to_string(), "sub/./.mdkg".to_string()];
        assert_eq!(
            p.mode_for("sub/.mdkg/work/a.md", &ws, false),
            X,
            "a workspace's nodes"
        );
        assert_eq!(
            p.mode_for("sub", &ws, true),
            X,
            "a folder holding a workspace"
        );
        assert_eq!(p.mode_for("sub/src", &ws, true), S);
    }

    #[test]
    fn a_folder_is_shared_only_when_everything_inside_would_be() {
        let p = policy(X, &[("tests/**", S), ("tests/golden/**", X), ("docs", S)]);
        assert_eq!(p.mode_for("tests/unit", &g(), true), S);
        assert_eq!(
            p.mode_for("tests", &g(), true),
            X,
            "tests/golden is exclusive"
        );
        assert_eq!(p.mode_for("tests/golden", &g(), true), X);
        assert_eq!(p.mode_for("docs", &g(), true), X, "docs/** isn't covered");
        assert_eq!(p.mode_for("docs", &g(), false), S, "docs as a file");

        // A shared default: a folder no glob matches is shared only when no
        // exclusive glob could match inside it (review of 1932559).
        let p = policy(S, &[("src/lib.rs", X)]);
        assert_eq!(p.mode_for("src", &g(), true), X);
        assert_eq!(p.mode_for("docs", &g(), true), S);
        assert_eq!(p.mode_for("src/main.rs", &g(), false), S);
        let p = policy(S, &[("docs", S), ("**/*.lock", X)]);
        assert_eq!(
            p.mode_for("docs", &g(), true),
            X,
            "a lock file may be inside"
        );
        assert_eq!(p.mode_for("docs", &g(), false), S);

        // A glob ending in `**` can match below a folder it starts at
        // (review round 2 of 1932559).
        let p = policy(S, &[("docs", S), ("docs/**", X)]);
        assert_eq!(p.mode_for("docs/a.md", &g(), false), X);
        assert_eq!(p.mode_for("docs", &g(), true), X);
        let p = policy(S, &[("*", S), ("*/**", X)]);
        assert_eq!(p.mode_for("src/a.rs", &g(), false), X);
        assert_eq!(p.mode_for("src", &g(), true), X);
    }

    #[test]
    fn the_policy_reads_from_config_and_rejects_bad_values() {
        let mut errors = Vec::new();
        let v: Value = serde_json::json!({"coordination": {"leases": {
            "default": "shared",
            "paths": [{"glob": "a", "mode": "exclusive"}],
            "unleased": "block"
        }}});
        let p = lease_policy(&v, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(p, policy(S, &[("a", X)]).with_unleased(Unleased::Block));

        let bad: Value = serde_json::json!({"coordination": {"leases": {
            "default": "advisory",
            "paths": [{"glob": "", "mode": "shared"}, {"glob": "b"}],
            "unleased": "ignore"
        }}});
        lease_policy(&bad, &mut errors);
        assert_eq!(errors.len(), 4, "{errors:?}");
        assert_eq!(
            lease_policy(&Value::Null, &mut errors),
            LeasePolicy::default()
        );
    }

    impl LeasePolicy {
        fn with_unleased(mut self, u: Unleased) -> Self {
            self.unleased = u;
            self
        }
    }
}
