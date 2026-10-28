//! Shared configuration layers and per-value origins. No writes or processes.
//!
//! Filesystem discovery takes captured home, XDG and Git environment inputs.
//! An absolute nonempty XDG_CONFIG_HOME wins over HOME/.config. Physical Git
//! discovery matches the store, independently of Git overrides; bare metadata
//! refuses, and conflicting overrides return warnings for CLI/doctor consumers.

use std::collections::BTreeMap;
use std::fmt;
#[cfg(feature = "settings-fs")]
use std::io::Read;
#[cfg(feature = "settings-fs")]
use std::path::Path;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

pub mod edit;
#[cfg(feature = "settings-fs")]
pub mod git;
mod json;

/// Maximum bytes read from any one configuration source.
pub const MAX_BYTES: u64 = 1 << 20;
/// Maximum container nesting in a configuration input.
pub const MAX_DEPTH: usize = 64;
pub const DEFAULTS_VERSION: &str = "0.7.0";
pub const DEFAULTS_JSON: &str = include_str!("defaults-0.7.0.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    BuiltIn,
    User,
    Project,
    Local,
    Flags,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Origin {
    pub layer: Layer,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

impl Origin {
    pub fn file(layer: Layer, path: impl Into<PathBuf>) -> Self {
        Self {
            layer,
            path: Some(path.into()),
        }
    }

    pub fn value(layer: Layer) -> Self {
        Self { layer, path: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Read,
    Invalid,
    TooLarge,
}

#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub origin: Origin,
    pub message: &'static str,
    cause: Option<std::io::Error>,
}

impl Error {
    fn invalid(origin: &Origin, message: &'static str) -> Self {
        Self {
            kind: ErrorKind::Invalid,
            origin: origin.clone(),
            message,
            cause: None,
        }
    }

    #[cfg(feature = "settings-fs")]
    fn read(origin: &Origin, error: impl Into<std::io::Error>) -> Self {
        Self {
            kind: ErrorKind::Read,
            origin: origin.clone(),
            message: "can't read the configuration source",
            cause: Some(error.into()),
        }
    }

    fn large(origin: &Origin) -> Self {
        Self {
            kind: ErrorKind::TooLarge,
            origin: origin.clone(),
            message: "configuration source exceeds the 1 MiB limit",
            cause: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.origin.path {
            write!(f, "{path:?}: ")?;
        }
        write!(f, "{:?} layer: {}", self.origin.layer, self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.as_ref().map(|e| e as &dyn std::error::Error)
    }
}

/// One parsed layer, or one committed definition represented as a patch.
#[derive(Debug, Clone)]
pub struct Input {
    pub origin: Origin,
    pub value: Value,
}

impl Input {
    pub fn new(origin: Origin, value: Value) -> Result<Self, Error> {
        let input = Self { origin, value };
        input.check()?;
        Ok(input)
    }

    fn check(&self) -> Result<(), Error> {
        if !self.value.is_object() {
            return Err(Error::invalid(
                &self.origin,
                "configuration layer must be a JSON object",
            ));
        }
        if self.origin.layer != Layer::Project {
            for (key, message) in [
                ("schema_version", "schema_version is project-only"),
                ("format_version", "format_version is project-only"),
                ("graph_id", "graph_id is project-only"),
            ] {
                if self.value.get(key).is_some() {
                    return Err(Error::invalid(&self.origin, message));
                }
            }
        }
        check_depth(&self.value, 0, &self.origin)
    }

    pub fn parse(origin: Origin, bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Error::large(&origin));
        }
        let value = json::parse(bytes).map_err(|_| {
            Error::invalid(
                &origin,
                "configuration must be valid JSON without duplicate keys",
            )
        })?;
        Self::new(origin, value)
    }

    /// None only for an absent file. A bad optional source must be visible.
    #[cfg(feature = "settings-fs")]
    pub fn read(layer: Layer, path: &Path) -> Result<Option<Self>, Error> {
        use rustix::fs::{Mode, OFlags};
        let origin = Origin::file(layer, path);
        let fd = match rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(e) => return Err(Error::read(&origin, e)),
        };
        let file = std::fs::File::from(fd);
        let meta = file.metadata().map_err(|e| Error::read(&origin, e))?;
        if !meta.is_file() {
            return Err(Error::invalid(
                &origin,
                "configuration source must be a regular file",
            ));
        }
        if meta.len() > MAX_BYTES {
            return Err(Error::large(&origin));
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::read(&origin, e))?;
        Self::parse(origin, &bytes).map(Some)
    }
}

fn check_depth(v: &Value, depth: usize, origin: &Origin) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::invalid(
            origin,
            "configuration exceeds the nesting limit",
        ));
    }
    match v {
        Value::Object(o) => {
            for v in o.values() {
                check_depth(v, depth + 1, origin)?;
            }
        }
        Value::Array(a) => {
            for v in a {
                check_depth(v, depth + 1, origin)?;
            }
        }
        _ => (),
    }
    Ok(())
}

pub fn defaults() -> Result<Input, Error> {
    Input::parse(Origin::value(Layer::BuiltIn), DEFAULTS_JSON.as_bytes())
}

/// Merged values, with an origin for every leaf (empty containers included).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Effective {
    pub values: Value,
    pub origins: BTreeMap<String, Origin>,
}

impl Effective {
    pub fn new(defaults: &Input) -> Result<Self, Error> {
        defaults.check()?;
        let mut origins = BTreeMap::new();
        record(&defaults.value, "", &defaults.origin, &mut origins);
        Ok(Self {
            values: defaults.value.clone(),
            origins,
        })
    }

    pub fn apply(&mut self, input: &Input) -> Result<(), Error> {
        // Inputs are public for inspection. Recheck before mutating so a
        // caller-modified input cannot partially apply an invalid patch.
        input.check()?;
        merge(
            &mut self.values,
            &input.value,
            "",
            &input.origin,
            &mut self.origins,
        );
        Ok(())
    }
}

fn pointer(parent: &str, name: &str) -> String {
    format!("{parent}/{}", name.replace('~', "~0").replace('/', "~1"))
}

fn record(v: &Value, path: &str, origin: &Origin, out: &mut BTreeMap<String, Origin>) {
    match v {
        Value::Object(o) if !o.is_empty() => {
            for (key, v) in o {
                record(v, &pointer(path, key), origin, out);
            }
        }
        Value::Array(a) if !a.is_empty() => {
            for (i, v) in a.iter().enumerate() {
                record(v, &pointer(path, &i.to_string()), origin, out);
            }
        }
        _ => {
            out.insert(path.into(), origin.clone());
        }
    }
}

fn merge(
    target: &mut Value,
    patch: &Value,
    path: &str,
    origin: &Origin,
    origins: &mut BTreeMap<String, Origin>,
) {
    if let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object()) {
        if !patch.is_empty() {
            origins.remove(path); // no longer an empty object
        }
        for (key, value) in patch {
            let path = pointer(path, key);
            if let Some(have) = target.get_mut(key) {
                merge(have, value, &path, origin, origins);
            } else {
                target.insert(key.clone(), value.clone());
                record(value, &path, origin, origins);
            }
        }
    } else {
        origins.retain(|key, _| {
            key != path && !key.strip_prefix(path).is_some_and(|s| s.starts_with('/'))
        });
        *target = patch.clone();
        record(target, path, origin, origins);
    }
}

/// Explicit paths keep tests from consulting the operator's home directory.
#[derive(Debug, Clone)]
pub struct Paths {
    pub user: Option<PathBuf>,
    pub project: PathBuf,
    pub local: Option<PathBuf>,
    #[cfg(feature = "settings-fs")]
    pub repository: Option<git::Repository>,
    #[cfg(feature = "settings-fs")]
    pub warnings: Vec<git::Warning>,
}

/// Captured environment values; omitted values never trigger ambient reads.
#[cfg(feature = "settings-fs")]
#[derive(Debug, Default, Clone, Copy)]
pub struct Environment<'a> {
    pub home: Option<&'a Path>,
    pub xdg_config_home: Option<&'a Path>,
    pub git: Option<git::Environment<'a>>,
}

#[cfg(feature = "settings-fs")]
impl Paths {
    /// Select the user file without consulting Git or requiring a project.
    pub fn user_path(env: Environment<'_>) -> Result<Option<PathBuf>, Error> {
        let user_base = env
            .xdg_config_home
            .filter(|p| !p.as_os_str().is_empty() && p.is_absolute());
        if let Some(home) = env
            .home
            .filter(|home| user_base.is_none() && !home.is_absolute())
        {
            return Err(Error::invalid(
                &Origin::file(Layer::User, home),
                "user home must be an absolute path",
            ));
        }
        Ok(user_base
            .map(|p| p.join("mdkg/config.json"))
            .or_else(|| env.home.map(|h| h.join(".config/mdkg/config.json"))))
    }

    pub fn discover(root: &Path, env: Environment<'_>) -> Result<Self, Error> {
        let user = Self::user_path(env)?;
        let origin = Origin::file(Layer::Project, root.join(".mdkg/config.json"));
        let root = std::fs::canonicalize(root).map_err(|e| Error::read(&origin, e))?;
        let repo = git::discover(&root).map_err(|e| {
            let mut error = Error::read(&origin, std::io::Error::other(e));
            error.message = git::CHECKOUT_REQUIRED;
            error
        })?;
        Ok(Self {
            user,
            project: root.join(".mdkg/config.json"),
            local: repo
                .as_ref()
                .map(|r| r.common.join("mdkg/config.local.json")),
            warnings: env
                .git
                .map(|e| git::warnings(repo.as_ref(), e))
                .unwrap_or_default(),
            repository: repo,
        })
    }

    pub fn read(&self) -> Result<Layers, Error> {
        Ok(Layers {
            user: self
                .user
                .as_deref()
                .map(|p| Input::read(Layer::User, p))
                .transpose()?
                .flatten(),
            project: Input::read(Layer::Project, &self.project)?,
            local: self
                .local
                .as_deref()
                .map(|p| Input::read(Layer::Local, p))
                .transpose()?
                .flatten(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Layers {
    pub user: Option<Input>,
    pub project: Option<Input>,
    pub local: Option<Input>,
}

impl Layers {
    /// Project additions (e.g. .mdkg/agents/*.json) belong above the project
    /// config, below machine-local overrides. Explicit flags are last.
    pub fn resolve(
        &self,
        defaults: &Input,
        additions: &[Input],
        flags: Option<&Value>,
    ) -> Result<Effective, Error> {
        for input in additions {
            if ["schema_version", "format_version", "graph_id"]
                .iter()
                .any(|key| input.value.get(key).is_some())
            {
                return Err(Error::invalid(
                    &input.origin,
                    "project additions cannot replace the physical project config's identity",
                ));
            }
        }
        let mut result = Effective::new(defaults)?;
        if defaults.origin.layer != Layer::BuiltIn {
            return Err(Error::invalid(
                &defaults.origin,
                "defaults must have a built-in origin",
            ));
        }
        for (input, expected) in self
            .user
            .iter()
            .map(|i| (i, Layer::User))
            .chain(self.project.iter().map(|i| (i, Layer::Project)))
            .chain(additions.iter().map(|i| (i, Layer::Project)))
            .chain(self.local.iter().map(|i| (i, Layer::Local)))
        {
            if input.origin.layer != expected {
                return Err(Error::invalid(
                    &input.origin,
                    "configuration source has the wrong layer origin",
                ));
            }
            result.apply(input)?;
        }
        if let Some(flags) = flags {
            result.apply(&Input::new(Origin::value(Layer::Flags), flags.clone())?)?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
