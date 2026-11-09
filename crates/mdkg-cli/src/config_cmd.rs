//! Inspect merged settings without opening the coordinator or writing
//! files (`config show`), and change the committed project config
//! (`config set`, `config unset`).

use crate::BIN;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::Subcommand;
use mdkg_format::Code;
use mdkg_format::settings::{self, Environment, Input, Layer, Layers, Paths};
use mdkg_graph::{GraphError, config::layers};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::output::{self, Report};

mod redact;
pub(crate) mod set;

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show merged settings and each value's source, without changing files.
    #[command(after_help = examples!("config show", "config show --effective --json"))]
    Show {
        /// Show the effective view (also the default without this flag).
        #[arg(long)]
        effective: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Change one key of the committed project config: in ref mode a
    /// commit on the graph ref, in checkout mode a commit of
    /// `.mdkg/config.json` where the committed config is read.
    ///
    /// The value is JSON (a bare word is taken as a string). The key must
    /// be one this build knows, and the changed config must pass the rules
    /// `doctor` and `validate` apply, or nothing changes.
    #[command(after_help = examples!(
        "config set coordination.messages.ack_grace_days 0",
        "config set coordination.leases.default '\"shared\"' --dry-run",
        "config set /coordination/messages/retention '\"durable\"' --json"
    ))]
    Set {
        /// A dotted path (`coordination.messages.ack_grace_days`) or a JSON
        /// pointer (`/coordination/messages/ack_grace_days`).
        key: String,
        /// The new value, as JSON (a negative number is a value, not a flag).
        #[arg(allow_hyphen_values = true)]
        value: String,
        /// Show the change, without claiming, writing or committing.
        #[arg(long)]
        dry_run: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove one key from the committed project config, so its default
    /// applies; committed as `config set` commits.
    #[command(after_help = examples!(
        "config unset coordination.messages.ack_grace_days",
        "config unset coordination.messages.ack_grace_days --dry-run --json"
    ))]
    Unset {
        /// A dotted path or a JSON pointer.
        key: String,
        /// Show the change, without claiming, writing or committing.
        #[arg(long)]
        dry_run: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

impl ConfigCommand {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Show { .. } => "config.show",
            Self::Set { .. } => "config.set",
            Self::Unset { .. } => "config.unset",
        }
    }

    pub fn json(&self) -> bool {
        match self {
            Self::Show { json, .. } | Self::Set { json, .. } | Self::Unset { json, .. } => *json,
        }
    }

    /// Whether it writes the graph (main's checkout-mode writer gate).
    pub fn writes_graph(&self) -> bool {
        matches!(
            self,
            Self::Set { dry_run: false, .. } | Self::Unset { dry_run: false, .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceLayer {
    BuiltIn,
    User,
    Project,
    Local,
    Flags,
}

impl From<Layer> for SourceLayer {
    fn from(layer: Layer) -> Self {
        match layer {
            Layer::BuiltIn => Self::BuiltIn,
            Layer::User => Self::User,
            Layer::Project => Self::Project,
            Layer::Local => Self::Local,
            Layer::Flags => Self::Flags,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct OriginData {
    pub layer: SourceLayer,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SourcePaths {
    /// Selected user location, including when its optional file is absent.
    pub user: Option<PathBuf>,
    /// The graph authority and pinned commit, or null when no project is selected.
    pub project: Option<PathBuf>,
    /// Selected machine-local location, including when its file is absent.
    pub local: Option<PathBuf>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ShowData {
    pub defaults_version: String,
    /// Versioned defaults and authored settings, merged by precedence.
    /// Conditional filesystem behavior remains with the settings' consumers.
    pub values: Value,
    /// Source of each leaf (including empty containers), keyed by JSON pointer.
    pub origins: BTreeMap<String, OriginData>,
    pub sources: SourcePaths,
    /// Configuration notices, including discovery and runtime limitations.
    pub warnings: Vec<String>,
    /// JSON pointers whose leaf values are hidden in both output formats.
    pub redacted: Vec<String>,
    /// In ref mode, the graph these settings were read from, as mdkg
    /// resolves it; absent in checkout mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph: Option<GraphData>,
    /// The coordinator store's own id, as `anchor` names it: stable for the
    /// store's life, across restores of its snapshots, and the same from
    /// every worktree of the repository. Null outside a repository, or
    /// before anything has created the store.
    pub store_id: Option<String>,
}

/// The coordinator store's id, read without creating or upgrading it.
pub(crate) fn store_id(root: &Path) -> Option<String> {
    let path = mdkg_coord_sqlite::git::store_path(root).ok()?;
    mdkg_coord_sqlite::Store::open_read_only(&path)
        .ok()??
        .store_id()
        .ok()
}

/// Where a ref-mode graph's committed config lives, from the resolver
/// every command uses, so a manual change targets exactly that
/// (Terra #15782).
#[derive(Debug, Serialize, JsonSchema)]
pub struct GraphData {
    /// The repository's common git folder: pass it as `git --git-dir`.
    pub common_dir: PathBuf,
    /// The graph ref's full name.
    #[serde(rename = "ref")]
    pub graph_ref: String,
    /// The commit the settings were read from.
    pub commit: String,
    /// The graph's root, repository-relative (`""` is the top).
    pub root: String,
    /// The graph folder under that root.
    pub dir: String,
    /// `<root>/<dir>/config.json` in the ref's tree.
    pub config_path: String,
}

/// Reuse the config display rules for other views of the same settings.
/// A key that looks like a credential is renamed, as `config show`
/// renames it; the returned renames map pointers into the original.
pub(crate) fn redact_display(value: &mut Value, pointer: &str) -> redact::Renames {
    redact::tree(value, pointer, &mut Vec::new())
}

pub(crate) fn display_text(text: &str) -> String {
    let mut value = Value::String(text.to_string());
    let _ = redact_display(&mut value, "");
    value.as_str().expect("display string").to_string()
}

/// Source-attributed settings used for a file-backed agent's next preparation.
#[derive(Debug, Serialize, JsonSchema)]
pub struct EffectiveAgentData {
    pub values: Value,
    /// Full config JSON pointers, matching `config show --effective`.
    pub origins: BTreeMap<String, OriginData>,
    pub redacted: Vec<String>,
}

pub(crate) fn effective_agent(
    snapshot: &mdkg_run::orchestration::runtime::Snapshot,
    name: &str,
) -> Option<EffectiveAgentData> {
    if !snapshot.definition_names.contains(name) {
        return None;
    }
    let root = format!("/agents/{}", name.replace('~', "~0").replace('/', "~1"));
    let mut values = snapshot.effective.values.get("agents")?.get(name)?.clone();
    let mut redacted = Vec::new();
    let renames = redact::tree(&mut values, &root, &mut redacted);
    let origins = snapshot
        .effective
        .origins
        .iter()
        .filter(|(p, _)| at_or_below(p, &root))
        .map(|(p, o)| {
            (
                renames.pointer(p),
                OriginData {
                    layer: o.layer.into(),
                    path: o.path.clone(),
                },
            )
        })
        .collect();
    Some(EffectiveAgentData {
        values,
        origins,
        redacted,
    })
}

fn at_or_below(pointer: &str, root: &str) -> bool {
    pointer == root
        || pointer
            .strip_prefix(root)
            .is_some_and(|s| s.starts_with('/'))
}

fn ancestor_notice(root: &Path) -> Vec<String> {
    for ancestor in root.ancestors().skip(1) {
        // Existence only: don't read or import the enclosing configuration.
        if std::fs::symlink_metadata(ancestor.join(".mdkg/config.json")).is_ok() {
            return vec![format!(
                "showing user-only settings; a project exists at {}; pass --root to select it",
                ancestor.display()
            )];
        }
    }
    Vec::new()
}

fn settings_error(error: settings::Error) -> GraphError {
    let code = match error.kind {
        settings::ErrorKind::Invalid => Code::ConfigInvalid,
        settings::ErrorKind::Read | settings::ErrorKind::TooLarge => Code::ConfigUnreadable,
    };
    let mut result = GraphError::failure(code, error.to_string());
    result.diagnostic.path = error.origin.path.map(|p| p.to_string_lossy().into_owned());
    result
}

/// Absence is not an unreadable, partial or linked project. In particular,
/// `is_file` would hide a dangling link and accidentally select the user view.
fn has_project(root: &Path) -> Result<bool, GraphError> {
    let mdkg = root.join(".mdkg");
    match std::fs::symlink_metadata(&mdkg) {
        Ok(meta) if meta.is_dir() => (),
        Ok(_) => {
            return Err(GraphError::failure(
                Code::ConfigUnreadable,
                ".mdkg must be a directory, not a link or file",
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => {
            return Err(GraphError::failure(
                Code::ConfigUnreadable,
                "cannot inspect the project configuration directory",
            ));
        }
    }
    // Capture will perform the bounded no-follow read. An existing .mdkg
    // without its config is a damaged/incomplete project, not a user-only view.
    Ok(true)
}

/// In ref mode the repository-top graph is authoritative even when this
/// checkout has no `.mdkg`. A non-Git folder keeps the user-only view, but a
/// present, unreadable Git marker is an error rather than an absent graph.
pub(crate) fn ref_mode_root(root: &Path) -> Result<Option<PathBuf>, GraphError> {
    match ref_mode_probe(root) {
        RefProbe::Checkout => Ok(None),
        RefProbe::Ref(top) => Ok(Some(top)),
        RefProbe::Unreadable(e) | RefProbe::Broken(e) => Err(e),
    }
}

/// The ref-mode root graph's folder. Ref mode keeps the repository-root
/// graph at `.mdkg` (graph_git's bind, `graphref::graph_paths`;
/// orchestrator #19410 on #16095): no mdkg writer records a root
/// `mdkg.graph.<id>.dir`, so only a hand-set key names another folder. So
/// doctor, config show, every bound command and the setup move into ref
/// mode refuse one naming another folder, saying where the graph's config
/// is on `tip` when there is one, and which key to unset.
/// Records that disagree keep `GraphRef::folder`'s own refusal.
pub(crate) fn root_graph_folder(
    top: &Path,
    graph: &mdkg_graph::graphref::GraphRef,
    tip: Option<&str>,
) -> Result<String, GraphError> {
    let dir = graph.folder("").map_err(|e| {
        GraphError::failure(
            Code::parse(e.code).unwrap_or(Code::ConfigInvalid),
            e.message,
        )
    })?;
    let default = mdkg_graph::committed::DEFAULT_DIR;
    if dir == default {
        return Ok(dir);
    }
    let keys = graph
        .graph_ids
        .iter()
        .filter(|id| graph.graph_roots.get(*id).is_none_or(String::is_empty))
        .filter(|id| graph.graph_dirs.get(*id) == Some(&dir))
        .map(|id| format!("mdkg.graph.{id}.dir"))
        .collect::<Vec<_>>();
    let key = if keys.is_empty() {
        "mdkg.graph.<id>.dir".to_owned()
    } else {
        keys.join(", ")
    };
    Err(unsupported_root_folder(top, &dir, &key, tip, &graph.name))
}

/// Whether the ref-mode clone at `top` registers its repository-root graph
/// in a folder [`root_graph_folder`] refuses (another folder, or records
/// that disagree), so no command binds it.
pub(crate) fn root_folder_unsupported(top: &Path) -> bool {
    matches!(
        mdkg_graph::graphref::resolve(top),
        Ok(Some(graph)) if !graph
            .folder("")
            .is_ok_and(|dir| dir == mdkg_graph::committed::DEFAULT_DIR)
    )
}

/// The refusal [`root_graph_folder`] and the setup move share: what is
/// registered, where the config is on the graph's tip, and the two ways on.
pub(crate) fn unsupported_root_folder(
    top: &Path,
    dir: &str,
    key: &str,
    tip: Option<&str>,
    graph_ref: &str,
) -> GraphError {
    let default = mdkg_graph::committed::DEFAULT_DIR;
    // Each key's own command (`key` may list several, comma-separated).
    let unset = key
        .split(", ")
        .map(|k| format!("`git config --unset {k}`"))
        .collect::<Vec<_>>()
        .join(" and ");
    let has = |folder: &str| {
        tip.and_then(|tip| {
            mdkg_graph::committed::config_file_if_present_dir(top, tip, "", folder)
                .ok()
                .map(|f| f.is_some())
        })
    };
    let found = match (has(dir), has(default)) {
        (Some(true), Some(true)) => format!(
            "; {graph_ref} has a config in both `{dir}` and `{default}`, so check which is current"
        ),
        (Some(true), _) => format!("; on {graph_ref} the graph's config is `{dir}/config.json`"),
        (Some(false), Some(true)) => {
            format!(
                "; on {graph_ref} the graph's config is `{default}/config.json`, not in `{dir}`"
            )
        }
        (Some(false), Some(false)) => {
            format!("; {graph_ref} has no config in `{dir}` or `{default}`")
        }
        _ => String::new(),
    };
    GraphError::failure(
        Code::ConfigInvalid,
        format!(
            "{key} registers the repository-root graph in `{dir}`, but ref mode keeps that \
             graph in `{default}`{found}"
        ),
    )
    .with_hint(match (has(dir), has(default)) {
        // The move is done on the ref: only the record is left (Terra
        // #16224).
        (Some(false), Some(true)) => format!(
            "the graph is already in `{default}` on {graph_ref}, so remove the key: {unset}; or \
             stay in checkout mode (`{BIN} install --checkout-mode` moves back)"
        ),
        _ => format!(
            "move the graph's `{dir}` folder to `{default}` in a commit to the graph, and only \
             then remove the key: {unset}; or stay in checkout mode (`{BIN} install \
             --checkout-mode` moves back)"
        ),
    })
}

/// [`root_graph_folder`] for the ref-mode graph at `top`, at its tip.
pub(crate) fn check_ref_root_folder(top: &Path) -> Result<(), GraphError> {
    let failed = |e: mdkg_graph::graphref::Error| {
        GraphError::failure(
            Code::parse(e.code).unwrap_or(Code::ConfigUnreadable),
            e.message,
        )
    };
    let trust = mdkg_coord_sqlite::git::WorktreeTrust::current();
    let Some(graph) = mdkg_graph::graphref::with_git_trust(trust.common, trust.worktree, || {
        mdkg_graph::graphref::resolve(top)
    })
    .map_err(failed)?
    else {
        return Ok(());
    };
    let tip = mdkg_graph::graphref::tip(top, &graph).ok();
    root_graph_folder(top, &graph, tip.as_deref()).map(drop)
}

/// [`GraphData`] for the ref-mode graph at the repository top `top`,
/// whose settings were read at `commit`.
fn graph_data(top: &Path, commit: String) -> Result<GraphData, GraphError> {
    let failed = |e: mdkg_graph::graphref::Error| {
        GraphError::failure(
            Code::parse(e.code).unwrap_or(Code::ConfigUnreadable),
            e.message,
        )
    };
    let trust = mdkg_coord_sqlite::git::WorktreeTrust::current();
    let graph = mdkg_graph::graphref::with_git_trust(trust.common, trust.worktree, || {
        mdkg_graph::graphref::resolve(top)
    })
    .map_err(failed)?
    .ok_or_else(|| GraphError::failure(Code::ConfigUnreadable, "ref mode is no longer set"))?;
    let dir = root_graph_folder(top, &graph, Some(&commit))?;
    Ok(GraphData {
        common_dir: graph.common,
        config_path: format!("{dir}/config.json"),
        graph_ref: graph.name,
        commit,
        root: String::new(),
        dir,
    })
}

/// What the entry gate learns about ref mode from `root`'s repository.
pub(crate) enum RefProbe {
    /// No repository, or one in checkout mode: the checkout's `.mdkg` is
    /// the graph.
    Checkout,
    /// Ref mode: the graph ref is the graph, and this is the repository top.
    Ref(PathBuf),
    /// Git can't read the repository itself (a stray or unreadable `.git`,
    /// a moved worktree, inherited trust pins that don't match), so the
    /// mode can't be known; the command's own checks report it.
    Unreadable(GraphError),
    /// The repository reads, but its graph-ref settings don't: ref mode may
    /// be configured, so no checkout copy may stand in for the graph.
    Broken(GraphError),
}

pub(crate) fn ref_mode_probe(root: &Path) -> RefProbe {
    let found = match settings::git::discover(root) {
        Ok(Some(found)) => found,
        Ok(None) => return RefProbe::Checkout,
        Err(e) => return RefProbe::Unreadable(GraphError::failure(Code::ConfigUnreadable, e)),
    };
    // The coordinator's own open, so the command's later open reuses its
    // checks instead of repeating them.
    let repo = match crate::coord::Repo::open(root) {
        Ok(repo) => repo,
        Err(e) => return refused_open(&found.common, e),
    };
    let trust = mdkg_coord_sqlite::git::WorktreeTrust::current();
    match mdkg_graph::graphref::with_git_trust(trust.common, trust.worktree, || {
        mdkg_graph::graphref::resolve(&repo.top)
    }) {
        Ok(Some(_)) => RefProbe::Ref(repo.top),
        Ok(None) => RefProbe::Checkout,
        Err(e) => RefProbe::Broken(GraphError::failure(
            Code::parse(e.code).unwrap_or(Code::ConfigUnreadable),
            e.message,
        )),
    }
}

/// The coordinator refused a repository git found (an old git, a worktree
/// link it doesn't trust, a skipped `.git`). Its common config, read through
/// git's isolated parser, says whether ref mode may be configured
/// (orchestrator #11014): a graph ref named, or a config that can't be read
/// or parsed, refuses with the open's error, so no checkout copy stands in.
/// No config, or no graph ref named, is checkout mode, and the command's own
/// checks report the refusal.
fn refused_open(common: &Path, open: GraphError) -> RefProbe {
    const CAP: u64 = 1 << 20;
    let bytes = match mdkg_git::read_capped(&common.join("config"), CAP + 1) {
        Ok(bytes) if bytes.len() as u64 <= CAP => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RefProbe::Checkout,
        _ => return RefProbe::Broken(open),
    };
    match mdkg_git::parse_config_snapshot(common, &bytes) {
        Ok(listed) if !names_graph_ref(&listed) => RefProbe::Checkout,
        _ => RefProbe::Broken(open),
    }
}

/// Whether `git config --null --list` output sets `mdkg.graph.ref`, in any
/// spelling git accepts for it (names are matched without regard to case,
/// which can only refuse more).
fn names_graph_ref(listed: &[u8]) -> bool {
    listed
        .split(|b| *b == 0)
        .filter_map(|entry| entry.split(|b| *b == b'\n').next())
        .any(|key| key.eq_ignore_ascii_case(b"mdkg.graph.ref"))
}

/// The settings `config show` reads, layered, before any display rule:
/// shared by `config show` and `agents list`.
struct Layered {
    effective: settings::Effective,
    sources: SourcePaths,
    warnings: Vec<String>,
    definition_names: BTreeSet<String>,
    authored_names: BTreeSet<String>,
    /// The names the project layer defines: `authored_names` less an
    /// inline entry that is `null` with no definition file, since a `null`
    /// removes an agent rather than defining it.
    project_defines: BTreeSet<String>,
    ref_root: Option<PathBuf>,
    read_commit: Option<String>,
}

fn layered(root: &Path) -> Result<Layered, GraphError> {
    if !std::fs::metadata(root)
        .map_err(|_| GraphError::failure(Code::IoReadFailed, "cannot inspect the selected root"))?
        .is_dir()
    {
        return Err(GraphError::failure(
            Code::ConfigInvalid,
            "the selected root must be a directory",
        ));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let git_dir = std::env::var_os("GIT_DIR").map(PathBuf::from);
    let git_common = std::env::var_os("GIT_COMMON_DIR").map(PathBuf::from);
    let cwd = std::env::current_dir().map_err(|_| {
        GraphError::failure(Code::IoReadFailed, "cannot determine the current directory")
    })?;
    let environment = Environment {
        home: home.as_deref(),
        xdg_config_home: xdg.as_deref(),
        git: Some(settings::git::Environment {
            cwd: &cwd,
            dir: git_dir.as_deref(),
            common_dir: git_common.as_deref(),
        }),
    };
    let ref_root = ref_mode_root(root)?;
    if let Some(top) = &ref_root {
        // Before any authority read, so a graph kept in another folder is
        // named rather than reported missing from `.mdkg` (#16095).
        check_ref_root_folder(top)?;
    }
    let project_root = ref_root.as_deref().unwrap_or(root);
    let has_project = ref_root.is_some() || has_project(root)?;
    let mut read_commit = None;
    let (effective, sources, warnings, definition_names, authored_names, project_defines) =
        if has_project {
            // Definitions and configuration share one committed authority. The
            // project identity is checked before any optional sources are read.
            let authority = mdkg_run::orchestration::authority(project_root)
                .map_err(crate::run::graph_error)?;
            read_commit = Some(authority.commit.clone());
            let project = layers::Project::from_authority(&authority, project_root, environment)?;
            let additions = mdkg_run::orchestration::agent::authority_layer_inputs(&authority)
                .map_err(crate::run::graph_error)?;
            let definition_names = additions
                .iter()
                .filter_map(|input| input.value.get("agents").and_then(Value::as_object))
                .flat_map(|agents| agents.keys().cloned())
                .collect::<BTreeSet<_>>();
            let authored_names = project
                .project_input()
                .value
                .get("agents")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|agents| agents.keys().cloned())
                .chain(definition_names.iter().cloned())
                .collect::<BTreeSet<_>>();
            let project_defines = project
                .project_input()
                .value
                .get("agents")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|agents| agents.iter())
                .filter(|(_, v)| !v.is_null())
                .map(|(k, _)| k.clone())
                .chain(definition_names.iter().cloned())
                .collect::<BTreeSet<_>>();
            let resolved =
                mdkg_run::orchestration::agent::resolve_layers(&project, &additions, None, None)?;
            let paths = project.paths();
            (
                resolved.effective,
                SourcePaths {
                    user: paths.user.clone(),
                    project: project.project_input().origin.path.clone(),
                    local: paths.local.clone(),
                },
                resolved.warnings.iter().map(ToString::to_string).collect(),
                definition_names,
                authored_names,
                project_defines,
            )
        } else {
            // A user-only view never imports an enclosing repository's local
            // settings or discovers Git. It also works outside a checkout.
            let user = Paths::user_path(environment).map_err(settings_error)?;
            let input = user
                .as_deref()
                .map(|p| Input::read(Layer::User, p))
                .transpose()
                .map_err(settings_error)?
                .flatten()
                .map(layers::normalize)
                .transpose()?;
            let effective = Layers {
                user: input,
                project: None,
                local: None,
            }
            .resolve(&settings::defaults().map_err(settings_error)?, &[], None)
            .map_err(settings_error)?;
            (
                effective,
                SourcePaths {
                    user,
                    project: None,
                    local: None,
                },
                ancestor_notice(root),
                BTreeSet::new(),
                BTreeSet::new(),
                BTreeSet::new(),
            )
        };
    Ok(Layered {
        effective,
        sources,
        warnings,
        definition_names,
        authored_names,
        project_defines,
        ref_root,
        read_commit,
    })
}

/// One agent the layered settings name, for `agents list`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ListedAgent {
    /// The agent's name; one that looks like a credential is
    /// `[redacted key <n>]`, numbered within this list.
    pub name: String,
    /// The layer that defines it: `project` for a committed definition
    /// file (`.mdkg/agents/<name>.json`) or an inline entry in the project
    /// config; otherwise the first of `user` and `local` that names it.
    pub defined_in: SourceLayer,
    /// Defined by its own committed file rather than inline in the config.
    pub file_backed: bool,
    /// Every layer whose own settings have an entry for it (the project
    /// layer: the project config or a definition file), in precedence order:
    /// a layer counts even where a higher one overrides each field it sets.
    pub layers: Vec<SourceLayer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The authored mail kinds that may start a turn; absent uses defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_kinds: Option<Vec<String>>,
}

/// The agents the settings `config show` reads name, with their layers;
/// values are redacted as `config show` redacts them. Reads settings
/// only: nothing is probed and no saved state is read.
pub(crate) fn agents(root: &Path) -> Result<(Vec<ListedAgent>, Vec<String>), GraphError> {
    let Layered {
        effective,
        sources,
        warnings,
        definition_names,
        project_defines,
        ..
    } = layered(root)?;
    // The names each layer's own input gives: an effective leaf keeps
    // only the winning layer, so a field every layer sets would hide the
    // others (Terra #20933). The project layer is the project config's
    // entries and the definition files. An entry that is `null` removes
    // the agent rather than defining it, so that layer doesn't count.
    let named_in = |layer: Layer, path: &Option<PathBuf>| -> BTreeSet<String> {
        path.as_deref()
            .and_then(|p| Input::read(layer, p).ok().flatten())
            .and_then(|input| {
                input
                    .value
                    .get("agents")
                    .and_then(Value::as_object)
                    .map(|a| {
                        a.iter()
                            .filter(|(_, v)| !v.is_null())
                            .map(|(k, _)| k.clone())
                            .collect()
                    })
            })
            .unwrap_or_default()
    };
    let by_layer = [
        (SourceLayer::User, named_in(Layer::User, &sources.user)),
        (SourceLayer::Project, project_defines.clone()),
        (SourceLayer::Local, named_in(Layer::Local, &sources.local)),
    ];
    let mut names: BTreeSet<String> = effective
        .values
        .get("agents")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|a| {
            a.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, _)| k.clone())
        })
        .collect();
    for (_, named) in &by_layer {
        names.extend(named.iter().cloned());
    }
    names.remove("defaults");
    // Each entry is read raw and redacted on its own, and a name that looks
    // like a credential gets a label from one count over this list that
    // skips every name shown as it is. No entry is looked up by a label,
    // so none can be mistaken for another (Terra #20949, #20956).
    let taken: BTreeSet<&String> = names
        .iter()
        .filter(|n| !redact::key_looks_secret(n))
        .collect();
    let mut count = 0;
    let mut listed = Vec::new();
    for name in &names {
        let at = format!("/agents/{}", name.replace('~', "~0").replace('/', "~1"));
        let layers: Vec<SourceLayer> = by_layer
            .iter()
            .filter(|(_, named)| named.contains(name))
            .map(|(layer, _)| *layer)
            .collect();
        let file_backed = definition_names.contains(name);
        let defined_in = if file_backed || project_defines.contains(name) {
            SourceLayer::Project
        } else {
            layers.first().copied().unwrap_or(SourceLayer::Project)
        };
        let secret_name = redact::key_looks_secret(name);
        let shown = effective.values.pointer(&at).cloned().map(|mut entry| {
            redact::entry(&mut entry, &at, secret_name);
            entry
        });
        let field = |f: &str| {
            shown
                .as_ref()
                .and_then(|e| e.get(f))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let shown_name = if secret_name {
            loop {
                count += 1;
                let label = redact::key_name(count);
                if !taken.contains(&label) {
                    break label;
                }
            }
        } else {
            name.clone()
        };
        listed.push(ListedAgent {
            name: shown_name,
            defined_in,
            file_backed,
            layers,
            harness: field("harness"),
            role: field("role"),
            wake_kinds: shown
                .as_ref()
                .and_then(|entry| entry.get("wake_kinds"))
                .and_then(|value| serde_json::from_value(value.clone()).ok()),
        });
    }
    let warnings = warnings.iter().map(|w| display_text(w)).collect();
    Ok((listed, warnings))
}

fn show(root: &Path) -> Result<Report<ShowData>, GraphError> {
    let Layered {
        effective,
        sources,
        mut warnings,
        definition_names,
        authored_names,
        ref_root,
        read_commit,
        ..
    } = layered(root)?;
    let optional = effective
        .origins
        .iter()
        .filter(|(_, o)| matches!(o.layer, Layer::User | Layer::Local));
    let mut notices = BTreeSet::new();
    let mut runtime_overrides = false;
    for (pointer, _) in optional {
        runtime_overrides |= at_or_below(pointer, "/orchestration");
        if at_or_below(pointer, "/projections") {
            notices.insert("projections are project-only: user and machine-local values shown here do not control shared repository files".to_string());
        }
        if at_or_below(pointer, "/coordination") {
            notices.insert("coordination is project-only: user and machine-local values shown here do not change mail or lease settings, which come from the committed project config".to_string());
        }
        if pointer == "/agents" {
            notices.insert(if definition_names.is_empty() {
                "optional agents root replacements are not applied in 0.7.0; only named overrides of file-backed definitions are supported"
            } else {
                "this optional agents root replacement removes a file-backed definition; runtime startup refuses it"
            }.to_string());
        }
        if let Some(tail) = pointer.strip_prefix("/agents/") {
            let name = tail
                .split('/')
                .next()
                .unwrap_or("")
                .replace("~1", "/")
                .replace("~0", "~");
            if name == "defaults" {
                notices.insert("agents.defaults from user and machine-local layers are not applied in 0.7.0; project defaults still apply".to_string());
            } else if !authored_names.contains(&name) {
                notices.insert(format!(
                    "agent {:?} is inactive: no graph definition exists for it",
                    display_text(&name)
                ));
            } else if !definition_names.contains(&name) {
                notices.insert(format!("optional overrides for agent {name:?} are not applied without a definition file; migrate a legacy agent with `{} agents edit <name>`", crate::BIN));
            } else {
                runtime_overrides = true;
            }
        }
    }
    if sources.project.is_some() && runtime_overrides {
        notices.insert("agents up captures these settings at startup; active watches retain that snapshot, including receiver policy; restart the watch to apply changes".to_string());
    }
    warnings.extend(notices);
    let mut values = effective.values;
    let mut redacted = Vec::new();
    let renames = redact::tree(&mut values, "", &mut redacted);
    let graph = match (&ref_root, read_commit) {
        (Some(top), Some(commit)) => Some(graph_data(top, commit)?),
        _ => None,
    };
    Ok(Report::ok(ShowData {
        defaults_version: settings::DEFAULTS_VERSION.into(),
        values,
        origins: effective
            .origins
            .into_iter()
            .map(|(pointer, origin)| {
                (
                    renames.pointer(&pointer),
                    OriginData {
                        layer: origin.layer.into(),
                        path: origin.path,
                    },
                )
            })
            .collect(),
        sources,
        warnings,
        redacted,
        graph,
        store_id: store_id(root),
    }))
}

pub fn run(root: &Path, actor: Option<String>, command: &ConfigCommand) -> u8 {
    let (key, change, dry_run) = match command {
        ConfigCommand::Show { .. } => return run_show(root, command),
        ConfigCommand::Set {
            key,
            value,
            dry_run,
            ..
        } => (key, set::Change::Set(value), *dry_run),
        ConfigCommand::Unset { key, dry_run, .. } => (key, set::Change::Unset, *dry_run),
    };
    output::emit(
        command.name(),
        command.json(),
        set::run(root, actor, key, &change, dry_run).unwrap_or_else(Report::failed),
        |data| {
            print!("{}", output::shown_body(&data.diff));
            let what = match (&data.commit, data.dry_run, data.changed) {
                (_, true, true) => "would change".to_owned(),
                (_, _, false) => "unchanged".to_owned(),
                (Some(commit), _, _) => format!("committed {commit}"),
                (None, _, _) => "changed".to_owned(),
            };
            let on = data
                .graph_ref
                .as_deref()
                .map(|r| format!(" on {}", output::shown(r)))
                .unwrap_or_default();
            println!("{}: {what}{on}", output::shown(&data.key));
        },
    )
}

fn run_show(root: &Path, command: &ConfigCommand) -> u8 {
    output::emit(
        command.name(),
        command.json(),
        show(root).unwrap_or_else(Report::failed),
        |data| {
            println!(
                "defaults version: {}",
                output::shown(&data.defaults_version)
            );
            println!(
                "sources: {}",
                output::shown(&serde_json::to_string(&data.sources).expect("source paths"))
            );
            if let Some(graph) = &data.graph {
                println!(
                    "graph: {}",
                    output::shown(&serde_json::to_string(graph).expect("graph"))
                );
            }
            for (pointer, origin) in &data.origins {
                let value = data.values.pointer(pointer).expect("origin names a leaf");
                println!(
                    "{} = {} ({})",
                    output::shown(&serde_json::to_string(pointer).expect("string")),
                    output::shown(&value.to_string()),
                    output::shown(&serde_json::to_string(origin).expect("origin"))
                );
            }
            for warning in &data.warnings {
                eprintln!(
                    "warning: {}",
                    output::shown(&serde_json::to_string(warning).expect("string"))
                );
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused() -> GraphError {
        GraphError::failure(Code::CoordStoreFailed, "the open's own refusal")
    }

    fn classify(config: Option<&[u8]>) -> &'static str {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-refused-open-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(config) = config {
            std::fs::write(dir.join("config"), config).unwrap();
        }
        let probe = refused_open(&dir, refused());
        std::fs::remove_dir_all(&dir).ok();
        match probe {
            RefProbe::Checkout => "checkout",
            RefProbe::Broken(e) => {
                assert_eq!(
                    e.diagnostic.code,
                    Code::CoordStoreFailed,
                    "the open's error"
                );
                "broken"
            }
            RefProbe::Ref(_) => "ref",
            RefProbe::Unreadable(_) => "unreadable",
        }
    }

    #[test]
    fn a_refused_open_refuses_whenever_ref_mode_may_be_configured() {
        assert_eq!(classify(None), "checkout", "no common config");
        assert_eq!(
            classify(Some(b"[core]\n\tbare = false\n")),
            "checkout",
            "no graph ref named"
        );
        for named in [
            &b"[mdkg \"graph\"]\n\tref = refs/heads/mdkg-graph\n"[..],
            b"[MDKG \"graph\"]\n\tREF = refs/heads/mdkg-graph\n",
            b"[mdkg.graph]\n\tref = refs/heads/mdkg-graph\n",
        ] {
            assert_eq!(
                classify(Some(named)),
                "broken",
                "{}",
                String::from_utf8_lossy(named)
            );
        }
        assert_eq!(classify(Some(b"[[[ not a config")), "broken", "unparseable");
        assert_eq!(
            classify(Some(&vec![b'#'; (1 << 20) + 1])),
            "broken",
            "over the cap"
        );
    }
}
