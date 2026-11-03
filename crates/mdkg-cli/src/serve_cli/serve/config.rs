//! `serve`'s settings (the serve edd's Configuration): read only from the
//! machine-local and user layers, never the committed project file. A
//! committed `serve` value is ignored, with a warning, as `run.wrapper`'s
//! is (A19).
//!
//! The config trust rule (orchestrator, #11337, #11395):
//! `<common>/mdkg/config.local.json` sits in the shared `.git` folder that
//! every worker can write, so it isn't the operator's voice. What widens
//! who can reach `serve` comes only from the user layer or the command
//! line:
//! - `serve.listen` from the machine-local layer is ignored, with a
//!   warning; `--listen` wins over both layers;
//! - `serve.limits` from the machine-local layer applies only where it's
//!   lower than the user layer's value, or the default; a higher one is
//!   ignored, with a warning;
//! - `drain_grace_s` comes from either layer, the machine-local one
//!   winning, within its bounds.
//!
//! serve-local (dec-01a18dd9) has no rates, origins, busy timeout or
//! lease cap of its own: each operation is the CLI's, with the CLI's
//! settings. Those keys were the networked design's, and are refused as
//! unknown.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mdkg_format::settings::{Environment, Input, Layer, Layers, Paths};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::GraphError;
use mdkg_serve::limits::{Limits, LimitsConfig};
use serde::Deserialize;
use serde_json::{Map, Value};

/// `serve.drain_grace_s`'s default (S3).
pub const DRAIN_GRACE_S: u64 = 120;
/// The bounds `drain_grace_s` must be within.
const DRAIN_GRACE_RANGE: std::ops::RangeInclusive<u64> = 0..=3_600;

/// The `serve` object, as either layer may hold part of it.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    listen: Option<String>,
    limits: Option<LimitsConfig>,
    drain_grace_s: Option<u64>,
}

/// The settings a start uses.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `serve.listen`; `--listen` replaces it.
    pub listen: Option<String>,
    pub limits: Limits,
    pub drain_grace: Duration,
    /// Whether the user layer names a `run.wrapper`, the only place it's
    /// read from: without one no turn's reads are sandboxed, so a loopback
    /// start warns that a turn could read the token (#12103).
    pub wrapper: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen: None,
            limits: Limits::default(),
            drain_grace: Duration::from_secs(DRAIN_GRACE_S),
            wrapper: false,
        }
    }
}

/// The environment the layers are found from, read once.
pub struct Env {
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            home: std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        }
    }
}

fn invalid(input: &Input, message: String) -> GraphError {
    let mut e = GraphError::usage(
        Code::ConfigInvalid,
        format!("{:?} layer: {message}", input.origin.layer),
    );
    e.diagnostic.path = input
        .origin
        .path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    e
}

/// Merges `top` over `base`, key by key, one level into objects: a layer
/// that sets `serve.limits.connections` keeps the other layer's
/// `serve.limits.body_bytes`.
fn overlay(base: &mut Map<String, Value>, top: &Map<String, Value>) {
    for (k, v) in top {
        match (base.get_mut(k), v) {
            (Some(Value::Object(b)), Value::Object(t)) => {
                for (k2, v2) in t {
                    b.insert(k2.clone(), v2.clone());
                }
            }
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

/// The `serve` object of a layer, refused unless it is an object.
fn serve_of(input: &Input) -> Result<Option<&Map<String, Value>>, GraphError> {
    match input.value.get("serve") {
        None => Ok(None),
        Some(Value::Object(m)) => Ok(Some(m)),
        Some(_) => Err(invalid(input, "serve must be an object".into())),
    }
}

/// Reads the settings for the graph at `root`, and the warnings to print:
/// one for a committed `serve` value, which is ignored. In ref mode
/// (`committed` is the graph ref's `.mdkg/config.json`, as the authority
/// reader read it) the checkout's file isn't the graph's, so that layer
/// isn't read at all, and the warning looks at the committed config
/// instead.
pub fn read(
    root: &Path,
    env: &Env,
    committed: Option<&Value>,
) -> Result<(Settings, Vec<Diagnostic>), GraphError> {
    let environment = Environment {
        home: env.home.as_deref(),
        xdg_config_home: env.xdg_config_home.as_deref(),
        git: None,
    };
    let unreadable = |e: mdkg_format::settings::Error| {
        GraphError::failure(Code::ConfigUnreadable, e.to_string())
    };
    let paths = Paths::discover(root, environment).map_err(unreadable)?;
    let layers = if committed.is_none() {
        paths.read().map_err(unreadable)?
    } else {
        let layer = |path: Option<&Path>, layer| {
            path.map(|p| Input::read(layer, p))
                .transpose()
                .map(Option::flatten)
                .map_err(unreadable)
        };
        Layers {
            user: layer(paths.user.as_deref(), Layer::User)?,
            project: None,
            local: layer(paths.local.as_deref(), Layer::Local)?,
        }
    };
    let user = user_config(&paths);
    let mut warnings = Vec::new();
    let committed_serve = match committed {
        Some(config) => config.get("serve").is_some(),
        None => layers
            .project
            .as_ref()
            .is_some_and(|p| p.value.get("serve").is_some()),
    };
    if committed_serve {
        warnings.push(
            Diagnostic::warning(
                Code::ConfigInvalid,
                "serve settings in the committed .mdkg/config.json are ignored",
            )
            .with_hint(format!(
                "set them in the user config ({user}); drain_grace_s and lower limits may \
                 also go in the machine-local config (<git common dir>/mdkg/config.local.json), \
                 which ignores serve.listen; or pass --listen"
            )),
        );
    }
    // Each layer is checked on its own first, so an unknown key, a wrong
    // type or a value out of range names the layer it's in; the merge
    // then only combines values already checked.
    let mut merged = Map::new();
    if let Some(user) = &layers.user
        && let Some(serve) = serve_of(user)?
    {
        let raw: Raw = serde_json::from_value(Value::Object(serve.clone()))
            .map_err(|e| invalid(user, format!("serve: {e}")))?;
        build(raw).map_err(|e| invalid(user, e))?;
        overlay(&mut merged, serve);
    }
    if let Some(local) = &layers.local
        && let Some(serve) = serve_of(local)?
    {
        let raw: Raw = serde_json::from_value(Value::Object(serve.clone()))
            .map_err(|e| invalid(local, format!("serve: {e}")))?;
        build(raw).map_err(|e| invalid(local, e))?;
        let so_far: Raw = serde_json::from_value(Value::Object(merged.clone()))
            .map_err(|e| GraphError::usage(Code::ConfigInvalid, format!("serve: {e}")))?;
        let so_far = build(so_far).map_err(|e| GraphError::usage(Code::ConfigInvalid, e))?;
        let (kept, ignored) = tightened(serve, &so_far);
        if !ignored.is_empty() {
            warnings.push(local_ignored(&ignored, &user));
        }
        overlay(&mut merged, &kept);
    }
    let raw: Raw = serde_json::from_value(Value::Object(merged))
        .map_err(|e| GraphError::usage(Code::ConfigInvalid, format!("serve: {e}")))?;
    let mut s = build(raw).map_err(|e| GraphError::usage(Code::ConfigInvalid, e))?;
    s.wrapper = layers.user.as_ref().is_some_and(|u| {
        u.value
            .get("run")
            .and_then(|r| r.get("wrapper"))
            .is_some_and(|w| !w.is_null())
    });
    Ok((s, warnings))
}

/// The machine-local layer's `serve` object as it may apply over
/// `so_far` (the user layer's settings, or the defaults), and the keys it
/// gave that are ignored: `listen` always, a limit only when it isn't
/// lower.
fn tightened(local: &Map<String, Value>, so_far: &Settings) -> (Map<String, Value>, Vec<String>) {
    let mut kept = Map::new();
    let mut ignored = Vec::new();
    for (k, v) in local {
        match k.as_str() {
            "listen" => ignored.push(format!("serve.{k}")),
            "limits" => {
                let mut part = Map::new();
                for (k2, v2) in v.as_object().into_iter().flatten() {
                    let current = match (k.as_str(), k2.as_str()) {
                        ("limits", "body_bytes") => so_far.limits.body_bytes as f64,
                        ("limits", "streams_per_machine") => {
                            so_far.limits.streams_per_machine as f64
                        }
                        ("limits", "connections") => so_far.limits.connections as f64,
                        // `Raw` already refused any other key.
                        _ => f64::NEG_INFINITY,
                    };
                    if v2.as_f64().is_some_and(|n| n <= current) {
                        part.insert(k2.clone(), v2.clone());
                    } else {
                        ignored.push(format!("serve.{k}.{k2}"));
                    }
                }
                kept.insert(k.clone(), Value::Object(part));
            }
            _ => {
                kept.insert(k.clone(), v.clone());
            }
        }
    }
    (kept, ignored)
}

/// The user config's path as this run resolves it (`$XDG_CONFIG_HOME`
/// honoured), for hints. With neither `HOME` nor an absolute
/// `XDG_CONFIG_HOME` no user layer is read at all, so the hint says so.
fn user_config(paths: &Paths) -> String {
    paths.user.as_deref().map_or_else(
        || "none is read: set HOME or an absolute XDG_CONFIG_HOME first".to_owned(),
        |p| p.display().to_string(),
    )
}

fn local_ignored(keys: &[String], user: &str) -> Diagnostic {
    Diagnostic::warning(
        Code::ConfigInvalid,
        format!(
            "{} in the machine-local config ignored: it may only tighten serve's limits",
            keys.join(", ")
        ),
    )
    .with_hint(format!(
        "<git common dir>/mdkg/config.local.json is writable by every worker; set serve.listen \
         and higher limits in the user config ({user}), or pass --listen"
    ))
}

/// `name` within `range`, or the refusal naming its bounds.
fn bounded(name: &str, v: u64, range: std::ops::RangeInclusive<u64>) -> Result<u64, String> {
    if range.contains(&v) {
        Ok(v)
    } else {
        Err(format!(
            "serve.{name} is {v}; it must be from {} to {}",
            range.start(),
            range.end()
        ))
    }
}

/// The settings `raw` gives over the defaults, every value checked.
fn build(raw: Raw) -> Result<Settings, String> {
    let mut s = Settings {
        listen: raw.listen,
        ..Settings::default()
    };
    if let Some(l) = &raw.limits {
        s.limits = Limits::from_config(l)?;
    }
    if let Some(g) = raw.drain_grace_s {
        // 0 is allowed: requests under way still get their 10 s; only
        // running turns lose their grace.
        s.drain_grace = Duration::from_secs(bounded("drain_grace_s", g, DRAIN_GRACE_RANGE)?);
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A git repository with a graph config, and a home of its own.
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "mdkg-serve-config-{}-{}",
                std::process::id(),
                uuid::Uuid::now_v7().simple()
            ));
            let repo = dir.join("repo");
            std::fs::create_dir_all(repo.join(".mdkg")).expect("repo");
            let out = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .output()
                .expect("git init");
            assert!(out.status.success(), "git init");
            std::fs::write(repo.join(".mdkg/config.json"), "{}").expect("config");
            Self { dir }
        }

        fn repo(&self) -> PathBuf {
            self.dir.join("repo")
        }

        fn env(&self) -> Env {
            Env {
                home: Some(self.dir.join("home")),
                xdg_config_home: None,
            }
        }

        fn user(&self, v: Value) {
            let p = self.dir.join("home/.config/mdkg/config.json");
            std::fs::create_dir_all(p.parent().expect("parent")).expect("user dir");
            std::fs::write(p, v.to_string()).expect("user config");
        }

        fn local(&self, v: Value) {
            let p = self.repo().join(".git/mdkg/config.local.json");
            std::fs::create_dir_all(p.parent().expect("parent")).expect("local dir");
            std::fs::write(p, v.to_string()).expect("local config");
        }

        fn project(&self, v: Value) {
            std::fs::write(self.repo().join(".mdkg/config.json"), v.to_string())
                .expect("project config");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// #16620: in ref mode the checkout's .mdkg/config.json isn't the
    /// graph's: a broken copy or a `serve` key in it neither refuses nor
    /// warns, and the user and local layers still apply. A `serve` key in
    /// the graph ref's committed config still warns (#16656).
    #[test]
    fn ref_mode_skips_the_checkouts_project_layer() {
        let f = Fixture::new();
        std::fs::write(f.repo().join(".mdkg/config.json"), "<<<<<<< conflict").expect("broken");
        f.local(serde_json::json!({"serve": {"drain_grace_s": 7}}));
        assert!(
            read(&f.repo(), &f.env(), None).is_err(),
            "checkout mode reads it"
        );
        let graph = serde_json::json!({"graph_id": "g"});
        let (s, warnings) = read(&f.repo(), &f.env(), Some(&graph)).expect("ref mode skips it");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(s.drain_grace, Duration::from_secs(7));
        f.project(serde_json::json!({"serve": {"listen": "127.0.0.1:1"}}));
        let (s, warnings) = read(&f.repo(), &f.env(), Some(&graph)).expect("read");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(s.listen, None);
        let graph = serde_json::json!({"graph_id": "g", "serve": {"listen": "127.0.0.1:1"}});
        let (s, warnings) = read(&f.repo(), &f.env(), Some(&graph)).expect("read");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].message.contains("committed"), "{warnings:?}");
        assert_eq!(s.listen, None);
    }

    /// #16696: both hints name the user config this run reads: under a
    /// custom XDG_CONFIG_HOME its path, and with no home at all, how to
    /// get one.
    #[test]
    fn hints_name_the_user_config_this_run_reads() {
        let f = Fixture::new();
        f.local(serde_json::json!({"serve": {"listen": "127.0.0.1:1"}}));
        let graph = serde_json::json!({"graph_id": "g", "serve": {}});
        let xdg = f.dir.join("xdg");
        let env = Env {
            home: Some(f.dir.join("home")),
            xdg_config_home: Some(xdg.clone()),
        };
        let (_, warnings) = read(&f.repo(), &env, Some(&graph)).expect("read");
        let want = xdg.join("mdkg/config.json").display().to_string();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        for w in &warnings {
            let hint = w.hint.as_deref().expect("hint");
            assert!(hint.contains(&want), "{hint}");
            assert!(!hint.contains("home/.config"), "{hint}");
        }
        let env = Env {
            home: None,
            xdg_config_home: None,
        };
        let (_, warnings) = read(&f.repo(), &env, Some(&graph)).expect("read");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        for w in &warnings {
            let hint = w.hint.as_deref().expect("hint");
            assert!(
                hint.contains("set HOME or an absolute XDG_CONFIG_HOME"),
                "{hint}"
            );
            assert!(!hint.contains("~/.config"), "{hint}");
        }
    }

    #[test]
    fn nothing_set_gives_the_defaults() {
        let f = Fixture::new();
        let (s, warnings) = read(&f.repo(), &f.env(), None).expect("read");
        assert!(warnings.is_empty());
        assert_eq!(s.listen, None);
        assert_eq!(s.drain_grace, Duration::from_secs(120));
        assert_eq!(s.limits, Limits::default());
    }

    #[test]
    fn the_local_layer_only_tightens_and_never_opens_a_network() {
        // The config trust rule (#11337, #11395).
        let f = Fixture::new();
        f.user(serde_json::json!({"serve": {
            "listen": "127.0.0.1:1",
            "drain_grace_s": 30,
            "limits": {"body_bytes": 1000, "connections": 10}
        }}));
        f.local(serde_json::json!({"serve": {
            "listen": "127.0.0.1:2",
            "drain_grace_s": 10,
            "limits": {"connections": 20, "body_bytes": 500, "streams_per_machine": 4}
        }}));
        let (s, warnings) = read(&f.repo(), &f.env(), None).expect("read");
        // Widening keys keep the user layer's values.
        assert_eq!(s.listen.as_deref(), Some("127.0.0.1:1"));
        assert_eq!(s.limits.connections, 10);
        // Lower ones apply, against the user's value or the default.
        assert_eq!(s.limits.body_bytes, 500);
        assert_eq!(s.limits.streams_per_machine, 4);
        // The grace comes from either layer.
        assert_eq!(s.drain_grace, Duration::from_secs(10));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let w = &warnings[0];
        assert_eq!(w.severity, mdkg_format::Severity::Warning);
        for key in ["serve.listen", "serve.limits.connections"] {
            assert!(w.message.contains(key), "{}", w.message);
        }
        assert!(!w.message.contains("body_bytes"), "{}", w.message);
        // With no user layer, the local one can't listen either.
        let g = Fixture::new();
        g.local(serde_json::json!({"serve": {"listen": "127.0.0.1:2"}}));
        let (s, warnings) = read(&g.repo(), &g.env(), None).expect("read");
        assert_eq!(s.listen, None);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn a_committed_value_is_ignored_with_a_warning() {
        let f = Fixture::new();
        f.project(serde_json::json!({"serve": {"listen": "0.0.0.0:9", "drain_grace_s": 1}}));
        let (s, warnings) = read(&f.repo(), &f.env(), None).expect("read");
        assert_eq!(s.listen, None);
        assert_eq!(s.drain_grace, Duration::from_secs(120));
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, Code::ConfigInvalid);
        assert_eq!(warnings[0].severity, mdkg_format::Severity::Warning);
    }

    #[test]
    fn an_unknown_key_or_a_bad_value_is_refused_naming_the_layer() {
        let f = Fixture::new();
        f.local(serde_json::json!({"serve": {"listne": "127.0.0.1:1"}}));
        let e = read(&f.repo(), &f.env(), None).expect_err("unknown key");
        assert_eq!(e.diagnostic.code, Code::ConfigInvalid);
        assert!(
            e.diagnostic.message.starts_with("Local layer"),
            "{}",
            e.diagnostic.message
        );
        f.local(serde_json::json!({"serve": {"limits": {"connections": 1}}}));
        assert!(read(&f.repo(), &f.env(), None).is_err());
        // The networked design's keys are unknown to serve-local.
        for key in [
            "rate",
            "allow_origins",
            "busy_timeout_ms",
            "max_lease_ttl_ms",
        ] {
            f.local(serde_json::json!({"serve": {key: 1}}));
            assert!(read(&f.repo(), &f.env(), None).is_err(), "{key}");
        }
        f.local(serde_json::json!({"serve": 3}));
        assert!(read(&f.repo(), &f.env(), None).is_err());
        f.local(serde_json::json!({"serve": {"drain_grace_s": 3601}}));
        let e = read(&f.repo(), &f.env(), None).expect_err("out of range");
        assert_eq!(e.diagnostic.code, Code::ConfigInvalid);
        assert!(
            e.diagnostic.message.contains("must be from"),
            "{}",
            e.diagnostic.message
        );
        f.local(serde_json::json!({"serve": {"drain_grace_s": 0}}));
        let (s, _) = read(&f.repo(), &f.env(), None).expect("in range");
        assert_eq!(s.drain_grace, Duration::ZERO);
    }

    #[test]
    fn a_bad_value_names_the_layer_it_is_in() {
        // #11385 item 2: a typo in the user layer, a valid local layer.
        let f = Fixture::new();
        f.user(serde_json::json!({"serve": {"listne": "127.0.0.1:1"}}));
        f.local(serde_json::json!({"serve": {"listen": "127.0.0.1:2"}}));
        let e = read(&f.repo(), &f.env(), None).expect_err("typo");
        assert!(
            e.diagnostic.message.starts_with("User layer"),
            "{}",
            e.diagnostic.message
        );
        assert!(
            e.diagnostic
                .path
                .as_deref()
                .is_some_and(|p| p.ends_with(".config/mdkg/config.json"))
        );
        f.user(serde_json::json!({"serve": {"drain_grace_s": 3601}}));
        let e = read(&f.repo(), &f.env(), None).expect_err("range");
        assert!(
            e.diagnostic.message.starts_with("User layer"),
            "{}",
            e.diagnostic.message
        );
        // The reverse: a good user layer, a typo in the local one (#11590
        // item 2).
        f.user(serde_json::json!({"serve": {"drain_grace_s": 60}}));
        f.local(serde_json::json!({"serve": {"drain_grase_s": 1}}));
        let e = read(&f.repo(), &f.env(), None).expect_err("local typo");
        assert!(
            e.diagnostic.message.starts_with("Local layer"),
            "{}",
            e.diagnostic.message
        );
        assert!(
            e.diagnostic
                .path
                .as_deref()
                .is_some_and(|p| p.ends_with("mdkg/config.local.json"))
        );
        f.local(serde_json::json!({"serve": {"drain_grace_s": 99999}}));
        let e = read(&f.repo(), &f.env(), None).expect_err("local range");
        assert!(
            e.diagnostic.message.starts_with("Local layer"),
            "{}",
            e.diagnostic.message
        );
    }
}
