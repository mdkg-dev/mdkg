//! Inspect merged settings without opening the coordinator or writing files.

use std::collections::BTreeMap;
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
}

impl ConfigCommand {
    pub fn name(&self) -> &'static str {
        "config.show"
    }

    pub fn json(&self) -> bool {
        match self {
            Self::Show { json, .. } => *json,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
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
    /// The physical project config, or null when no project is selected.
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

fn show(root: &Path) -> Result<Report<ShowData>, GraphError> {
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
    let (effective, sources, mut warnings) = if has_project(root)? {
        // Physical identity precedes optional sources, including definitions.
        let project = layers::Project::capture(root, environment)?;
        let additions =
            mdkg_run::orchestration::agent::layer_inputs(root).map_err(crate::run::graph_error)?;
        let resolved =
            mdkg_run::orchestration::agent::resolve_layers(&project, &additions, None, None)?;
        let paths = project.paths();
        (
            resolved.effective,
            SourcePaths {
                user: paths.user.clone(),
                project: Some(paths.project.clone()),
                local: paths.local.clone(),
            },
            resolved.warnings.iter().map(ToString::to_string).collect(),
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
        )
    };
    // Remove this notice when all runtime consumers use the same layered
    // settings, including installation, diagnostics, skills and hooks.
    if effective.origins.iter().any(|(pointer, origin)| {
        matches!(origin.layer, Layer::User | Layer::Local)
            && ["/agents", "/orchestration", "/projections"]
                .into_iter()
                .any(|root| {
                    pointer == root
                        || pointer
                            .strip_prefix(root)
                            .is_some_and(|tail| tail.starts_with('/'))
                })
    }) {
        warnings.push("settings (agents, orchestration, projections) from user or machine-local layers are displayed here but are not yet applied by runtime commands in this build".into());
    }
    let mut values = effective.values;
    let mut redacted = Vec::new();
    redact::values(&mut values, "", false, &mut redacted);
    Ok(Report::ok(ShowData {
        defaults_version: settings::DEFAULTS_VERSION.into(),
        values,
        origins: effective
            .origins
            .into_iter()
            .map(|(pointer, origin)| {
                (
                    pointer,
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
    }))
}

pub fn run(root: &Path, command: &ConfigCommand) -> u8 {
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
