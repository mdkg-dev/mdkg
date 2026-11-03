//! The launch wrapper, `run.wrapper` (API v1 A19): an outside isolation
//! layer (a container, a sandbox runner, a remote shell) that runs each
//! turn's harness for mdkg. mdkg keeps its one spawn point and puts the
//! wrapper in front of it:
//!
//! `<command…> <spec path> -- <harness program> <harness args…>`
//!
//! with the harness's own cwd, environment, stdin, stdout and stderr, in
//! the turn's process group, under the turn's own timeout. The spec
//! (`mdkg.run.wrapper.v1`, a 0600 file in the turn's run folder) says what
//! the turn is and which paths it needs. Off by default.
//!
//! Configured only in the user layer (`$XDG_CONFIG_HOME/mdkg/config.json`,
//! else `~/.config/mdkg/config.json`), outside every path a turn's spec
//! grants. A value anywhere in the repository is ignored with a warning:
//! in the committed config or an agent definition, a committed argv would
//! run on every clone that pulls it; in the machine-local layer
//! (`<git common dir>/mdkg/config.local.json`), a worker, which may write
//! the common folder, could rewrite the wrapper that isolates it
//! (orchestrator, on #11337). The program, as the lease authority's, is an
//! absolute path outside the repository, to a file owned by the user (or
//! by root) that no one else can write.
//!
//! The wrapper may refuse a turn by exiting 125 before it starts the
//! harness: `run.wrapper_refused`, and the agent isn't run again. mdkg
//! keeps owning the timeout, the process-group kill (`timeout_grace_ms`
//! after a gentle stop), output parsing and receipts.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use mdkg_format::settings::{self, Input, Layer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::RunError;
use crate::profile::{AgentCommand, Harness, Role};

pub const SCHEMA: &str = "mdkg.run.wrapper.v1";
/// The exit status a wrapper refuses a turn with.
pub const REFUSED: i32 = 125;
pub const CODE_REFUSED: &str = "run.wrapper_refused";
const CODE_CONFIG: &str = "config.invalid";
const DEFAULT_GRACE_MS: u64 = 5_000;
const MAX_GRACE_MS: u64 = 600_000;

/// A configured wrapper, its program checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapper {
    /// As configured: the program, then its own arguments.
    pub command: Vec<String>,
    /// The program as checked, links resolved: what runs.
    pub program: PathBuf,
    /// How long the turn's process group has after a gentle stop.
    pub grace: Duration,
    /// The user config file it was read from, which no spec ever grants
    /// write on, nor its folder.
    pub user_config: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configured {
    command: Vec<String>,
    #[serde(default)]
    timeout_grace_ms: Option<u64>,
}

/// The wrapper the configuration names, if any, and warnings about values
/// it ignored.
#[derive(Debug, Default)]
pub struct Loaded {
    pub wrapper: Option<Wrapper>,
    pub warnings: Vec<String>,
}

fn invalid(message: impl Into<String>) -> RunError {
    RunError::usage(CODE_CONFIG, message)
        .with_hint("run.wrapper is {\"command\": [\"/abs/program\", args…], \"timeout_grace_ms\": 5000} in your user config")
}

/// Reads `run.wrapper` for the repository at `root`. Fails closed: a
/// wrapper that's configured but can't be used refuses the turn rather
/// than run it without its isolation.
pub fn load(root: &Path) -> Result<Loaded, RunError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let git_dir = std::env::var_os("GIT_DIR").map(PathBuf::from);
    let git_common = std::env::var_os("GIT_COMMON_DIR").map(PathBuf::from);
    let cwd = std::env::current_dir()
        .map_err(|e| RunError::io("can't determine the current directory", e))?;
    let environment = settings::Environment {
        home: home.as_deref(),
        xdg_config_home: xdg.as_deref(),
        git: Some(settings::git::Environment {
            cwd: &cwd,
            dir: git_dir.as_deref(),
            common_dir: git_common.as_deref(),
        }),
    };
    let paths = settings::Paths::discover(root, environment)
        .map_err(|e| invalid(format!("can't find the config layers: {e}")))?;
    let mut loaded = Loaded::default();
    if let Ok(Some(project)) = Input::read(Layer::Project, &paths.project)
        && project.value.pointer("/run/wrapper").is_some()
    {
        loaded.warnings.push(
            "run.wrapper in the committed config is ignored: set it in your user config".into(),
        );
    }
    let definitions = crate::orchestration::agent::layer_inputs(root).unwrap_or_default();
    if definitions.iter().any(|input| {
        input
            .value
            .get("agents")
            .and_then(Value::as_object)
            .is_some_and(|agents| agents.values().any(|a| a.pointer("/run/wrapper").is_some()))
    }) {
        loaded.warnings.push(
            "run.wrapper in an agent definition is ignored: set it in your user config".into(),
        );
    }
    if let Some(local) = &paths.local
        && let Ok(Some(input)) = Input::read(Layer::Local, local)
        && input.value.pointer("/run/wrapper").is_some()
    {
        loaded.warnings.push(
            "run.wrapper in the machine-local config is ignored: a turn can write that file; set it in your user config"
                .into(),
        );
    }
    let Some(path) = &paths.user else {
        return Ok(loaded);
    };
    let input = Input::read(Layer::User, path)
        .map_err(|e| invalid(format!("can't read {}: {e}", path.display())))?;
    let Some(value) = input
        .and_then(|i| i.value.pointer("/run/wrapper").cloned())
        .filter(|v| !v.is_null())
    else {
        return Ok(loaded);
    };
    let configured: Configured = serde_json::from_value(value)
        .map_err(|e| invalid(format!("{}: run.wrapper: {e}", path.display())))?;
    let configs: Vec<PathBuf> = [Harness::Claude, Harness::Codex, Harness::OpenCode]
        .into_iter()
        .filter_map(|h| harness_config(h, &[]))
        .collect();
    let mut wrapper = check(root, configured, path, &configs)?;
    wrapper.user_config = Some(path.clone());
    loaded.wrapper = Some(wrapper);
    Ok(loaded)
}

/// `configs`: every harness's config folder, which the program must lie
/// outside.
fn check(
    root: &Path,
    configured: Configured,
    path: &Path,
    configs: &[PathBuf],
) -> Result<Wrapper, RunError> {
    let at = path.display();
    if configured.command.is_empty() || configured.command.iter().any(String::is_empty) {
        return Err(invalid(format!(
            "{at}: run.wrapper.command must be a non-empty list of non-empty arguments"
        )));
    }
    let grace = configured.timeout_grace_ms.unwrap_or(DEFAULT_GRACE_MS);
    if !(1..=MAX_GRACE_MS).contains(&grace) {
        return Err(invalid(format!(
            "{at}: run.wrapper.timeout_grace_ms must be 1 to {MAX_GRACE_MS}"
        )));
    }
    // An absolute program outside the repository: never one found on a
    // PATH or under a worktree, which an agent or a PR could plant.
    let program = Path::new(&configured.command[0]);
    if !program.is_absolute() {
        return Err(invalid(format!(
            "{at}: run.wrapper's program must be an absolute path, not `{}`",
            configured.command[0]
        )));
    }
    let real = fs::canonicalize(program)
        .map_err(|e| invalid(format!("{at}: can't find {}: {e}", program.display())))?;
    let meta = fs::metadata(&real)
        .map_err(|e| invalid(format!("{at}: can't inspect {}: {e}", real.display())))?;
    let uid = rustix::process::geteuid().as_raw();
    if !meta.is_file() || meta.mode() & 0o111 == 0 {
        return Err(invalid(format!(
            "{at}: {} isn't an executable file",
            real.display()
        )));
    }
    if (meta.uid() != uid && meta.uid() != 0) || meta.mode() & 0o022 != 0 {
        return Err(invalid(format!(
            "{at}: {} must be owned by you (or root) and writable by no one else",
            real.display()
        )));
    }
    let mut inside = Vec::new();
    if let Ok(common) = mdkg_coord_sqlite::git::common_dir(root) {
        inside.push(fs::canonicalize(&common).unwrap_or(common));
    }
    inside.extend(
        mdkg_coord_sqlite::git::worktrees(root)
            .unwrap_or_default()
            .into_iter()
            .map(|w| fs::canonicalize(&w).unwrap_or(w)),
    );
    inside.push(fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()));
    if inside.iter().any(|dir| real.starts_with(dir)) {
        return Err(invalid(format!(
            "{at}: run.wrapper's program {} is inside the repository",
            real.display()
        ))
        .with_hint("install the wrapper outside the repository and its worktrees"));
    }
    // Nor in a harness's config folder, which a worker's turn may write:
    // the turn could replace the program that isolates the next one.
    if let Some(folder) = configs
        .iter()
        .find(|c| within(program, c) || within(&real, c))
    {
        return Err(in_harness_config(&at.to_string(), &real, folder));
    }
    Ok(Wrapper {
        command: configured.command,
        program: real,
        grace: Duration::from_millis(grace),
        user_config: None,
    })
}

/// Whether `path` is `folder` or inside it, as written or resolved.
fn within(path: &Path, folder: &Path) -> bool {
    path.starts_with(folder) || fs::canonicalize(folder).is_ok_and(|f| path.starts_with(f))
}

fn in_harness_config(at: &str, program: &Path, folder: &Path) -> RunError {
    invalid(format!(
        "{at}: run.wrapper's program {} is inside {}, a harness's config folder a worker's turn may write",
        program.display(),
        folder.display()
    ))
    .with_hint("install the wrapper where no turn can write, such as /usr/local/bin")
}

/// A path the turn needs, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Access {
    pub path: PathBuf,
    /// `read` or `write`.
    pub access: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Paths {
    pub worktree: Option<Access>,
    pub git_common_dir: Option<Access>,
    pub graph_checkout: Option<Access>,
    pub run_dir: Option<Access>,
    pub scratch_shared: Option<Access>,
    pub scratch_actor: Option<Access>,
    pub harness_config: Option<Access>,
}

/// The turn spec, `mdkg.run.wrapper.v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Spec {
    pub schema: &'static str,
    pub turn_id: String,
    pub agent: String,
    pub harness: Harness,
    pub role: Role,
    pub mode: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env_keys: Vec<String>,
    pub paths: Paths,
    pub read_only: bool,
    pub timeout_secs: Option<u64>,
}

/// What a turn knows about itself for its spec, before its argv.
#[derive(Debug, Clone)]
pub struct Turn {
    pub turn_id: String,
    pub agent: String,
    pub harness: Harness,
    pub role: Role,
    pub mode: String,
    pub common: Option<PathBuf>,
    pub run_dir: PathBuf,
    pub graph_checkout: Option<PathBuf>,
    pub timeout: Option<Duration>,
}

/// The harness launch a spec describes: what spawns without a wrapper.
#[derive(Debug, Clone)]
pub struct Launch<'a> {
    pub program: &'a Path,
    pub args: &'a [String],
    pub cwd: &'a Path,
    /// The variables mdkg sets, with their values (only names reach the spec).
    pub env: &'a [(String, String)],
    /// The names of the variables only the harness's own process gets.
    pub secret_keys: Vec<String>,
}

impl<'a> Launch<'a> {
    pub fn of(cmd: &'a AgentCommand, args: &'a [String]) -> Self {
        Self {
            program: &cmd.program,
            args,
            cwd: &cmd.cwd,
            env: &cmd.env,
            secret_keys: cmd.secret_env.iter().map(|(k, _)| k.clone()).collect(),
        }
    }
}

/// A wrapper ready to run one turn.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub wrapper: Wrapper,
    pub turn: Turn,
}

/// What the receipt records: that a wrapper ran, which program, and the
/// spec it was given, by digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ran {
    pub command: [PathBuf; 1],
    pub spec_digest: String,
}

impl Prepared {
    /// The spec for `cmd` run with `args`. Workers may write the paths
    /// they need; a reviewer's are all `read`.
    pub fn spec(&self, launch: &Launch<'_>) -> Spec {
        let t = &self.turn;
        let read_only = t.role == Role::Reviewer;
        let access = if read_only { "read" } else { "write" };
        // Never write on the user config that names the wrapper, its folder
        // or a folder above them. Paths compare as written and resolved, so
        // a symlink on either side can't hide an ancestor.
        let resolved = |p: &Path| fs::canonicalize(p).ok();
        // Nor on the wrapper's program or its folder.
        let mut guarded: Vec<PathBuf> = Vec::new();
        for program in [
            self.wrapper.program.clone(),
            PathBuf::from(&self.wrapper.command[0]),
        ] {
            guarded.extend(program.parent().map(Path::to_path_buf));
            guarded.push(program);
        }
        if let Some(c) = &self.wrapper.user_config {
            let parent = c.parent().unwrap_or(c);
            guarded.extend([c.clone(), parent.to_path_buf()]);
            guarded.extend(resolved(parent));
            if let Some(real) = resolved(c) {
                guarded.extend(real.parent().map(Path::to_path_buf));
                guarded.push(real);
            }
        }
        let at = |path: PathBuf, access: &'static str| {
            let real = resolved(&path);
            let covers = |g: &PathBuf| {
                g.starts_with(&path) || real.as_ref().is_some_and(|r| g.starts_with(r))
            };
            let access = if guarded.iter().any(covers) {
                "read"
            } else {
                access
            };
            Some(Access { path, access })
        };
        let mut env_keys: Vec<String> = launch
            .env
            .iter()
            .map(|(k, _)| k.clone())
            .chain(launch.secret_keys.iter().cloned())
            .collect();
        env_keys.sort();
        env_keys.dedup();
        let mut argv = vec![launch.program.to_string_lossy().into_owned()];
        argv.extend(launch.args.iter().cloned());
        Spec {
            schema: SCHEMA,
            turn_id: t.turn_id.clone(),
            agent: t.agent.clone(),
            harness: t.harness,
            role: t.role,
            mode: t.mode.clone(),
            argv,
            cwd: launch.cwd.to_path_buf(),
            env_keys,
            paths: Paths {
                worktree: at(launch.cwd.to_path_buf(), access),
                git_common_dir: t.common.clone().and_then(|c| at(c, access)),
                graph_checkout: t.graph_checkout.clone().and_then(|g| at(g, access)),
                run_dir: at(t.run_dir.clone(), access),
                scratch_shared: t
                    .common
                    .as_ref()
                    .and_then(|c| at(c.join("mdkg/scratch/shared"), access)),
                scratch_actor: t.common.as_ref().and_then(|c| {
                    at(
                        c.join("mdkg/scratch")
                            .join(crate::state::file_name(&t.agent)),
                        access,
                    )
                }),
                harness_config: harness_config(t.harness, launch.env).and_then(|p| at(p, access)),
            },
            read_only,
            timeout_secs: t.timeout.map(|d| d.as_secs()),
        }
    }

    /// Writes the spec to a new 0600 file beside the turn's logs, and the
    /// argv that runs the wrapper in front of the harness.
    pub fn write(
        &self,
        stem: &Path,
        launch: &Launch<'_>,
    ) -> Result<(Vec<std::ffi::OsString>, Ran), RunError> {
        // The turn's own harness config folder, as its environment sets
        // it, may differ from the ones `load` checked. Known only here, at
        // the spawn and after admission, so it refuses as the wrapper's own
        // exit 125 does: `run.wrapper_refused`, the harness never started
        // (review #12074).
        if let Some(folder) = harness_config(self.turn.harness, launch.env) {
            let program = &self.wrapper.program;
            if within(program, &folder) || within(Path::new(&self.wrapper.command[0]), &folder) {
                // Where it was set, as `load`'s refusals say (review #12262).
                let at = self
                    .wrapper
                    .user_config
                    .as_ref()
                    .map_or_else(|| "run.wrapper".into(), |c| c.display().to_string());
                return Err(RunError::failure(
                    CODE_REFUSED,
                    format!(
                        "{at}: run.wrapper's program {} is inside {}, the turn's own harness config folder, which a worker's turn may write",
                        program.display(),
                        folder.display()
                    ),
                )
                .with_hint("install the wrapper where no turn can write, such as /usr/local/bin"));
            }
        }
        let spec = self.spec(launch);
        let bytes = serde_json::to_vec_pretty(&spec)
            .map_err(|e| RunError::failure("io.write_failed", e.to_string()))?;
        let path = stem.with_extension("wrapper.json");
        // A failed turn's logs stay until the next success; a spec there is
        // an older turn's.
        match fs::remove_file(&path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(RunError::io("can't replace the wrapper's turn spec", e)),
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&path)
            .map_err(|e| RunError::io("can't write the wrapper's turn spec", e))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| RunError::io("can't write the wrapper's turn spec", e))?;
        let mut argv: Vec<std::ffi::OsString> =
            self.wrapper.command[1..].iter().map(Into::into).collect();
        argv.push(path.into_os_string());
        argv.push("--".into());
        argv.push(launch.program.as_os_str().to_owned());
        argv.extend(launch.args.iter().map(Into::into));
        let ran = Ran {
            command: [self.wrapper.program.clone()],
            spec_digest: format!("sha256:{}", mdkg_format::Digest::of_bytes(&bytes).hex()),
        };
        Ok((argv, ran))
    }
}

/// The harness's own config folder, which holds its login and sessions:
/// where its environment points, else its default under the home folder.
fn harness_config(harness: Harness, set: &[(String, String)]) -> Option<PathBuf> {
    let env = |key: &str| {
        set.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| PathBuf::from(v))
            .or_else(|| std::env::var_os(key).map(PathBuf::from))
            .filter(|p| p.is_absolute())
    };
    let home = env("HOME");
    match harness {
        Harness::Claude => env("CLAUDE_CONFIG_DIR").or_else(|| home.map(|h| h.join(".claude"))),
        Harness::Codex => env("CODEX_HOME").or_else(|| home.map(|h| h.join(".codex"))),
        Harness::OpenCode => env("XDG_CONFIG_HOME")
            .or_else(|| home.map(|h| h.join(".config")))
            .map(|c| c.join("opencode")),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENV: &[(&str, &str)] = &[("MDKG_RUN_TURN", "t"), ("CODEX_HOME", "/tmp/codex-home")];

    fn env() -> Vec<(String, String)> {
        ENV.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn launch<'a>(cwd: &'a Path, args: &'a [String], env: &'a [(String, String)]) -> Launch<'a> {
        Launch {
            program: Path::new("/usr/bin/true"),
            args,
            cwd,
            env,
            secret_keys: vec!["ANTHROPIC_API_KEY".into()],
        }
    }

    fn prepared(role: Role, harness: Harness) -> Prepared {
        Prepared {
            wrapper: Wrapper {
                command: vec!["/usr/bin/env".into(), "--profile".into()],
                program: PathBuf::from("/usr/bin/env"),
                grace: Duration::from_millis(DEFAULT_GRACE_MS),
                user_config: Some(PathBuf::from("/home/u/.config/mdkg/config.json")),
            },
            turn: Turn {
                turn_id: "01a18cb6-0000-7000-8000-000000000000".into(),
                agent: "rev".into(),
                harness,
                role,
                mode: "auto".into(),
                common: Some(PathBuf::from("/repo/.git")),
                run_dir: PathBuf::from("/repo/.git/mdkg/run"),
                graph_checkout: Some(PathBuf::from("/repo/.worktrees/graph")),
                timeout: Some(Duration::from_secs(900)),
            },
        }
    }

    #[test]
    fn a_reviewers_paths_are_all_read_and_a_workers_are_write() {
        let cwd = Path::new("/repo/.worktrees/rev");
        let args = vec!["exec".to_string(), "--json".to_string()];
        let env = env();
        let spec = prepared(Role::Reviewer, Harness::Codex).spec(&launch(cwd, &args, &env));
        assert!(spec.read_only);
        assert_eq!(spec.argv, ["/usr/bin/true", "exec", "--json"]);
        // Names only, the key's too; never a value.
        assert_eq!(
            spec.env_keys,
            ["ANTHROPIC_API_KEY", "CODEX_HOME", "MDKG_RUN_TURN"]
        );
        assert_eq!(spec.timeout_secs, Some(900));
        let value = serde_json::to_value(&spec).unwrap();
        for (name, path) in value["paths"].as_object().unwrap() {
            assert_eq!(path["access"], "read", "{name}");
        }
        assert_eq!(value["paths"]["harness_config"]["path"], "/tmp/codex-home");
        assert_eq!(
            value["paths"]["scratch_actor"]["path"],
            "/repo/.git/mdkg/scratch/rev"
        );
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["harness"], "codex");
        assert_eq!(value["role"], "reviewer");
        let worker = prepared(Role::Worker, Harness::Claude).spec(&launch(cwd, &args, &env));
        assert!(!worker.read_only);
        let value = serde_json::to_value(&worker).unwrap();
        for (name, path) in value["paths"].as_object().unwrap() {
            assert_eq!(path["access"], "write", "{name}");
        }
        // A worker never gets write on the user config, its folder or a
        // folder above them: here OpenCode's config folder is set to the
        // user config home, and the home itself is the worktree.
        let guarded = [
            ("XDG_CONFIG_HOME".to_string(), "/home/u/.config".to_string()),
            ("HOME".to_string(), "/home/u".to_string()),
        ];
        let home = Path::new("/home/u");
        let spec = prepared(Role::Worker, Harness::OpenCode).spec(&launch(home, &args, &guarded));
        let value = serde_json::to_value(&spec).unwrap();
        assert_eq!(
            value["paths"]["harness_config"]["path"],
            "/home/u/.config/opencode"
        );
        assert_eq!(value["paths"]["harness_config"]["access"], "write");
        assert_eq!(value["paths"]["worktree"]["access"], "read", "{value}");
        let config = Path::new("/home/u/.config/mdkg/config.json");
        for (name, path) in value["paths"].as_object().unwrap() {
            let at = Path::new(path["path"].as_str().unwrap());
            if config.starts_with(at) {
                assert_eq!(path["access"], "read", "{name}");
            }
        }
    }

    #[test]
    fn a_symlink_on_the_user_config_path_cant_hide_it_from_the_write_guard() {
        let dir = std::env::temp_dir().join(format!("mdkg-wrapper-{}", uuid::Uuid::now_v7()));
        let real = dir.join("real");
        fs::create_dir_all(real.join("cfg/mdkg")).unwrap();
        fs::write(real.join("cfg/mdkg/config.json"), "{}").unwrap();
        std::os::unix::fs::symlink(&real, dir.join("link")).unwrap();
        let real = fs::canonicalize(&real).unwrap();
        let mut p = prepared(Role::Worker, Harness::Claude);
        p.wrapper.user_config = Some(dir.join("link/cfg/mdkg/config.json"));
        let args = Vec::new();
        let env = env();
        // The worktree is the resolved folder above the config: read only.
        let spec = p.spec(&launch(&real, &args, &env));
        let value = serde_json::to_value(&spec).unwrap();
        assert_eq!(value["paths"]["worktree"]["access"], "read", "{value}");
        // And given through the link, the same.
        let linked = dir.join("link");
        let spec = p.spec(&launch(&linked, &args, &env));
        let value = serde_json::to_value(&spec).unwrap();
        assert_eq!(value["paths"]["worktree"]["access"], "read", "{value}");
        // A sibling folder stays writable.
        fs::create_dir_all(dir.join("other")).unwrap();
        let other = dir.join("other");
        let spec = p.spec(&launch(&other, &args, &env));
        let value = serde_json::to_value(&spec).unwrap();
        assert_eq!(value["paths"]["worktree"]["access"], "write", "{value}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_wrapper_argv_is_its_own_then_the_spec_then_the_harness_unchanged() {
        let dir = std::env::temp_dir().join(format!("mdkg-wrapper-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        let stem = dir.join("turn-3");
        let args = vec!["-p".to_string(), "a b".to_string()];
        let p = prepared(Role::Worker, Harness::OpenCode);
        fs::write(stem.with_extension("wrapper.json"), "an older turn's").unwrap();
        let env = env();
        let (argv, ran) = p.write(&stem, &launch(&dir, &args, &env)).unwrap();
        let spec_path = stem.with_extension("wrapper.json");
        assert_eq!(
            argv,
            [
                "--profile".into(),
                spec_path.clone().into_os_string(),
                "--".into(),
                "/usr/bin/true".into(),
                "-p".into(),
                "a b".into(),
            ]
        );
        let meta = fs::metadata(&spec_path).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);
        let bytes = fs::read(&spec_path).unwrap();
        assert_eq!(
            ran.spec_digest,
            format!("sha256:{}", mdkg_format::Digest::of_bytes(&bytes).hex())
        );
        assert_eq!(ran.command, [PathBuf::from("/usr/bin/env")]);
        let spec: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            spec["argv"],
            serde_json::json!(["/usr/bin/true", "-p", "a b"])
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_program_must_be_absolute_trusted_and_outside_the_repository() {
        let dir = std::env::temp_dir().join(format!("mdkg-wrapper-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let configured = |command: &[&str], grace: Option<u64>| Configured {
            command: command.iter().map(|s| s.to_string()).collect(),
            timeout_grace_ms: grace,
        };
        let ok = check(&dir, configured(&["/bin/sh", "-c"], None), &path, &[]).unwrap();
        assert_eq!(ok.grace, Duration::from_millis(DEFAULT_GRACE_MS));
        for bad in [
            configured(&[], None),
            configured(&["sh"], None),
            configured(&["/bin/sh", ""], None),
            configured(&["/bin/sh"], Some(0)),
            configured(&["/no/such/wrapper"], None),
        ] {
            assert!(check(&dir, bad, &path, &[]).is_err());
        }
        // Inside the repository's folder.
        let inside = dir.join("wrap");
        fs::write(&inside, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&inside, fs::Permissions::from_mode(0o755)).unwrap();
        let e = check(
            &dir,
            configured(&[inside.to_str().unwrap()], None),
            &path,
            &[],
        )
        .unwrap_err();
        assert!(e.message.contains("inside the repository"), "{e:?}");
        // Writable by others.
        let elsewhere = std::env::temp_dir().join(format!("mdkg-wrap-{}", uuid::Uuid::now_v7()));
        fs::write(&elsewhere, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o777)).unwrap();
        let e = check(
            &dir,
            configured(&[elsewhere.to_str().unwrap()], None),
            &path,
            &[],
        )
        .unwrap_err();
        assert!(e.message.contains("writable by no one else"), "{e:?}");
        fs::remove_file(&elsewhere).ok();
        fs::remove_dir_all(&dir).ok();
    }

    /// A wrapper under a harness's config folder, which a worker's turn
    /// may write, is refused when it's loaded and when a turn's own folder
    /// holds it; and no spec grants write on the program or its folder
    /// (review #11906).
    #[test]
    fn a_wrapper_under_a_harness_config_folder_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("mdkg-wrapper-{}", uuid::Uuid::now_v7()));
        let repo = dir.join("repo");
        let claude = dir.join("home/.claude");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(claude.join("bin")).unwrap();
        std::os::unix::fs::symlink(&claude, dir.join("claude-link")).unwrap();
        let program = claude.join("bin/isolate");
        fs::write(&program, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("config.json");
        let configured = |program: &Path| Configured {
            command: vec![program.to_str().unwrap().into()],
            timeout_grace_ms: None,
        };
        // As written, through a link to the folder, or the folder named
        // through a link.
        for (program, folder) in [
            (program.clone(), claude.clone()),
            (dir.join("claude-link/bin/isolate"), claude.clone()),
            (program.clone(), dir.join("claude-link")),
        ] {
            let e = check(&repo, configured(&program), &path, &[folder]).unwrap_err();
            assert!(e.message.contains("config folder"), "{e:?}");
        }
        assert!(
            check(
                &repo,
                configured(&program),
                &path,
                &[dir.join("home/.codex")]
            )
            .is_ok()
        );
        // A turn whose harness config folder holds the program.
        let mut p = prepared(Role::Worker, Harness::Codex);
        p.wrapper.program = program.clone();
        p.wrapper.command = vec![program.to_str().unwrap().into()];
        let cwd = Path::new("/repo/.worktrees/rev");
        let args: Vec<String> = Vec::new();
        let set = vec![(
            "CODEX_HOME".to_string(),
            claude.to_str().unwrap().to_string(),
        )];
        let e = p
            .write(&dir.join("turn-1"), &launch(cwd, &args, &set))
            .unwrap_err();
        assert_eq!(e.code, CODE_REFUSED, "{e:?}");
        assert!(e.message.contains("config folder"), "{e:?}");
        assert!(
            e.message.starts_with("/home/u/.config/mdkg/config.json: "),
            "{e:?}"
        );
        assert!(!dir.join("turn-1.wrapper.json").exists());
        // Nor write on its folder, even as a path a turn needs.
        let mut t = p.clone();
        t.turn.graph_checkout = Some(claude.join("bin"));
        let spec = t.spec(&launch(cwd, &args, &env()));
        assert_eq!(spec.paths.graph_checkout.unwrap().access, "read");
        assert_eq!(spec.paths.harness_config.unwrap().access, "write");
        fs::remove_dir_all(&dir).ok();
    }
}
