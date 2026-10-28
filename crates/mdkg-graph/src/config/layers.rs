//! Captured graph configuration, ready for CLI and runtime consumers.
//!
//! Callers supply environment paths explicitly. Project identity is checked
//! before optional files are read, and the same captured project value is
//! merged and validated. No store opens, writes or process-environment reads.

use std::path::{Path, PathBuf};

use mdkg_format::settings::{self, Effective, Environment, Input, Layer, Layers, Origin, Paths};
use mdkg_format::{Code, settings::git};
use serde_json::Value;

use super::{CONFIG_PATH, Config};
use crate::error::GraphError;

/// Identity-checked project bytes and the paths discovered for this operation.
/// Capture again for each new operation; this is a snapshot, not a file watch.
#[derive(Debug)]
pub struct Project {
    paths: Paths,
    input: Input,
    root: PathBuf,
}

/// Effective JSON and its origins can be shown even if a setting fails typed
/// validation. [`Self::config`] performs that validation for runtime consumers.
#[derive(Debug)]
pub struct Resolved {
    pub effective: Effective,
    pub warnings: Vec<git::Warning>,
    root: PathBuf,
    work_dir: Option<PathBuf>,
}

fn settings_error(error: settings::Error) -> GraphError {
    let code = match error.kind {
        settings::ErrorKind::Invalid => Code::ConfigInvalid,
        settings::ErrorKind::Read | settings::ErrorKind::TooLarge => Code::ConfigUnreadable,
    };
    let path = error
        .origin
        .path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let mut error = GraphError::failure(code, error.to_string());
    error.diagnostic.path = path;
    error
}

fn invalid(origin: &Origin, message: &'static str) -> GraphError {
    let mut error = GraphError::failure(
        Code::ConfigInvalid,
        format!("{:?} layer: {message}", origin.layer),
    );
    error.diagnostic.path = origin
        .path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    error
}

/// Normalize aliases before merging: a higher layer replaces a lower layer's
/// storage choice, while conflicting spellings within one source are invalid.
pub fn normalize(mut input: Input) -> Result<Input, GraphError> {
    let Some(work) = input.value.get_mut("work").and_then(Value::as_object_mut) else {
        return Ok(input);
    };
    let alias = match work.get("committed") {
        None => None,
        Some(Value::String(s)) if s == "none" => Some("local"),
        Some(Value::String(s)) if s == "definitions" => Some("git"),
        Some(_) => {
            return Err(invalid(
                &input.origin,
                "work.committed must be none or definitions",
            ));
        }
    };
    let location = match work.get("location") {
        None => None,
        Some(Value::String(s)) if s == "local" => Some("local"),
        Some(Value::String(s)) if s == "git" => Some("git"),
        Some(_) => return Err(invalid(&input.origin, "work.location must be local or git")),
    };
    if alias.zip(location).is_some_and(|(a, b)| a != b) {
        return Err(invalid(
            &input.origin,
            "work.location conflicts with its work.committed alias",
        ));
    }
    if let Some(alias) = alias {
        work.insert("location".into(), Value::String(alias.into()));
        work.remove("committed");
    }
    Ok(input)
}

impl Project {
    /// Read only the physical project input and check its identity. No optional
    /// user/local source can rescue or mask an unsupported project identity.
    pub fn capture(root: &Path, environment: Environment<'_>) -> Result<Self, GraphError> {
        let paths = Paths::discover(root, environment).map_err(settings_error)?;
        let input = Input::read(Layer::Project, &paths.project)
            .map_err(settings_error)?
            .ok_or_else(|| {
                GraphError::failure(Code::ConfigUnreadable, "project config is absent")
                    .at(CONFIG_PATH, None)
            })?;
        super::check_schema_version(&input.value, false)?;
        super::read_identity(&input.value)?;
        let root = paths
            .project
            .parent()
            .and_then(Path::parent)
            .expect("project path has .mdkg parent")
            .to_path_buf();
        Ok(Self { paths, input, root })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// The exact identity-checked project input captured for this operation.
    /// Consumers with executable settings can reject project-authored fields
    /// before merging, including fields a local override would later mask.
    pub fn project_input(&self) -> &Input {
        &self.input
    }

    /// Merge per-file definitions at project priority, below local overrides.
    /// Flags are patches of explicitly supplied options, never clap defaults.
    pub fn resolve(
        &self,
        additions: &[Input],
        flags: Option<&Value>,
    ) -> Result<Resolved, GraphError> {
        self.resolve_with(&[], additions, flags)
    }

    /// Preview a full replacement for exactly one file before a scoped setter
    /// writes it. Project identity must be identical to the captured input.
    pub fn resolve_replacing(
        &self,
        replacement: &Input,
        additions: &[Input],
        flags: Option<&Value>,
    ) -> Result<Resolved, GraphError> {
        self.resolve_replacements(std::slice::from_ref(replacement), additions, flags)
    }

    /// Resolve captured in-memory replacements without rereading those files.
    /// This permits a caller's project-layer transformation alongside a scoped
    /// user/local edit preview. Each layer may occur once; all paths and project
    /// identity are checked before any optional source is read.
    pub fn resolve_replacements(
        &self,
        replacements: &[Input],
        additions: &[Input],
        flags: Option<&Value>,
    ) -> Result<Resolved, GraphError> {
        for (index, replacement) in replacements.iter().enumerate() {
            if replacements[..index]
                .iter()
                .any(|other| other.origin.layer == replacement.origin.layer)
            {
                return Err(invalid(&replacement.origin, "duplicate replacement layer"));
            }
            self.check_replacement(replacement)?;
        }
        self.resolve_with(replacements, additions, flags)
    }

    fn check_replacement(&self, replacement: &Input) -> Result<(), GraphError> {
        let expected = match replacement.origin.layer {
            Layer::Project => Some(&self.paths.project),
            Layer::User => self.paths.user.as_ref(),
            Layer::Local => self.paths.local.as_ref(),
            _ => None,
        };
        if expected.is_none() || replacement.origin.path.as_ref() != expected {
            return Err(invalid(
                &replacement.origin,
                "replacement does not name a captured configuration source",
            ));
        }
        let replacement = Input::new(replacement.origin.clone(), replacement.value.clone())
            .map_err(settings_error)?;
        if replacement.origin.layer == Layer::Project
            && ["schema_version", "format_version", "graph_id"]
                .iter()
                .any(|key| replacement.value.get(key) != self.input.value.get(key))
        {
            return Err(invalid(
                &replacement.origin,
                "project identity cannot be changed by a setter",
            ));
        }
        Ok(())
    }

    fn resolve_with(
        &self,
        replacements: &[Input],
        additions: &[Input],
        flags: Option<&Value>,
    ) -> Result<Resolved, GraphError> {
        let read = |layer, path: Option<&Path>| -> Result<Option<Input>, GraphError> {
            if let Some(input) = replacements.iter().find(|r| r.origin.layer == layer) {
                return normalize(input.clone()).map(Some);
            }
            path.map(|p| Input::read(layer, p).map_err(settings_error))
                .transpose()?
                .flatten()
                .map(normalize)
                .transpose()
        };
        let layers = Layers {
            user: read(Layer::User, self.paths.user.as_deref())?,
            project: Some(normalize(
                replacements
                    .iter()
                    .find(|r| r.origin.layer == Layer::Project)
                    .unwrap_or(&self.input)
                    .clone(),
            )?),
            local: read(Layer::Local, self.paths.local.as_deref())?,
        };
        let additions = additions
            .iter()
            .cloned()
            .map(normalize)
            .collect::<Result<Vec<_>, _>>()?;
        let flags = flags
            .map(|v| {
                Input::new(Origin::value(Layer::Flags), v.clone())
                    .map_err(settings_error)
                    .and_then(normalize)
            })
            .transpose()?;
        let effective = layers
            .resolve(
                &settings::defaults().map_err(settings_error)?,
                &additions,
                flags.as_ref().map(|f| &f.value),
            )
            .map_err(settings_error)?;
        let work_dir = self.paths.repository.as_ref().map(|repo| {
            let prefix = repo
                .prefix
                .components()
                .map(|p| p.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            crate::store_work::under(&repo.common, &prefix)
        });
        Ok(Resolved {
            effective,
            warnings: self.paths.warnings.clone(),
            root: self.root.clone(),
            work_dir,
        })
    }
}

impl Resolved {
    /// Validate the captured merged settings, without rereading configuration
    /// or rediscovering a different common directory during this operation.
    pub fn config(&self) -> Result<Config, GraphError> {
        super::from_value(&self.root, self.effective.values.clone(), false, || {
            Ok(self.work_dir.clone())
        })
        .map_err(|mut e| {
            if e.code() == Code::ConfigInvalid {
                // This is a merged value, not necessarily a project-file
                // error. Its origins remain available for repair tooling.
                e.diagnostic.path = None;
                e.diagnostic.message = format!("effective configuration: {}", e.diagnostic.message);
            }
            e
        })
    }
}

pub fn load(
    root: &Path,
    environment: Environment<'_>,
    additions: &[Input],
    flags: Option<&Value>,
) -> Result<Resolved, GraphError> {
    Project::capture(root, environment)?.resolve(additions, flags)
}

#[cfg(test)]
mod tests;
