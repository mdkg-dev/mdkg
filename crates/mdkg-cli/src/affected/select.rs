//! The selection engine: each changed path goes through the global files,
//! the config rules and the language adapters, and anything none of them
//! accounts for forces a full run.

use std::collections::{BTreeMap, BTreeSet};

use super::config::{Config, Rule, RuleSelect, RustTarget};
use super::narrow::Narrowing;
use super::{GoSelection, MatchKind, ReasonCode, RustTestSelection, Selected};

/// One test target.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Go {
        module: String,
        dir: String,
        package: String,
    },
    Rust(String),
    RustTest(RustTarget),
    Suite(String),
}

impl Target {
    /// How `paths[].selects` names it.
    pub fn label(&self) -> String {
        match self {
            Target::Go { package, .. } => format!("go:{package}"),
            Target::Rust(c) => format!("rust:{c}"),
            // Keep the shipped path/package projection compatible.
            Target::RustTest(t) => format!("rust:{}", t.package),
            Target::Suite(s) => format!("suite:{s}"),
        }
    }
}

/// What an adapter says about a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mapping {
    /// Not this adapter's path.
    NotMine,
    /// This adapter's path; run these (the path's own package or crate and
    /// everything that depends on it).
    Selects(BTreeSet<Target>),
    /// As `Selects`, with whole crates where finer targets can't be
    /// derived, and why: a rule naming finer targets still wins.
    Coarse(BTreeSet<Target>, String),
    /// A deleted path whose package or crate is gone at the head.
    Removed(String),
    /// This adapter's kind of path, but nothing at the head builds it (for
    /// example a `.go` file no package includes on this platform).
    Unaccounted(String),
}

/// A language adapter: maps paths to the targets that depend on them.
pub trait Adapter {
    /// `go` or `rust`, as `paths[].matched_by` names it.
    fn kind(&self) -> MatchKind;
    /// The adapter's name in reasons.
    fn name(&self) -> &'static str;
    /// Maps one repository-relative path. `deleted` says the path doesn't
    /// exist at the head. An `Err` makes the run full.
    fn map(&mut self, path: &str, deleted: bool) -> Result<Mapping, String>;
    /// Resolves a rule's targets for this adapter (`go` patterns or `rust`
    /// crate names). An unknown target is an `Err`, so a stale rule fails
    /// closed.
    fn resolve(&mut self, names: &[String]) -> Result<BTreeSet<Target>, String>;
    /// Resolves a rule's Rust test targets. A target the head doesn't have
    /// is listed in `missing`, not an `Err`: the shared config may name a
    /// target that an older head predates.
    fn resolve_tests(&mut self, _targets: &[RustTarget]) -> Result<TestResolution, String> {
        Err("this adapter cannot resolve Rust test targets".into())
    }
}

/// A rule's Rust test targets at the head.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TestResolution {
    pub found: BTreeSet<Target>,
    pub missing: Vec<RustTarget>,
}

/// The engine's verdict for one path.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PathVerdict {
    pub matched_by: BTreeSet<MatchKind>,
    pub selects: BTreeSet<Target>,
    /// The reasons it forces a full run, with detail.
    pub reasons: BTreeMap<ReasonCode, String>,
    /// Whole crates used because a path lacks a finer rule: the path, the
    /// crate, and why the adapter couldn't derive finer targets, if it
    /// said.
    pub fallback_crates: BTreeSet<(String, String, Option<String>)>,
    /// Rule targets the head doesn't have, skipped: the path whose rule
    /// named it (the old side of a rename keeps its own path), the rule
    /// as messages name it, the target, and whether none of the rule's
    /// targets exist, so its packages run whole.
    pub missing_targets: BTreeSet<(String, String, RustTarget, bool)>,
}

pub struct Engine<'a> {
    pub config: &'a Config,
    pub go: Option<&'a mut dyn Adapter>,
    pub rust: Option<&'a mut dyn Adapter>,
    /// Built-in global files whose change was confined, and the base's
    /// rules when a config edit was narrowed.
    pub narrowing: Narrowing,
}

impl Engine<'_> {
    /// Judges one path: `deleted` when it doesn't exist at the head.
    ///
    /// When a config edit was narrowed, the path is also judged under the
    /// base's rules on their own, and both verdicts count: a rule the range
    /// adds or changes can't narrow the range's own selection.
    pub fn judge(&mut self, path: &str, deleted: bool) -> PathVerdict {
        let Engine {
            config,
            go,
            rust,
            narrowing,
        } = self;
        let narrowed = narrowing.paths.get(path);
        let changed = narrowed.map(Vec::as_slice).unwrap_or_default();
        let mut v = judge_with(
            config,
            "",
            narrowed.is_some(),
            changed,
            go,
            rust,
            path,
            deleted,
        );
        if let Some(base) = &narrowing.base {
            let b = judge_with(
                base,
                "the base's ",
                narrowed.is_some(),
                &[],
                go,
                rust,
                path,
                deleted,
            );
            v.matched_by.extend(b.matched_by);
            v.selects.extend(b.selects);
            for (code, detail) in b.reasons {
                v.reasons.entry(code).or_insert(detail);
            }
            v.fallback_crates.extend(b.fallback_crates);
            v.missing_targets.extend(b.missing_targets);
        }
        v
    }
}

/// Judges one path under one set of rules. `whose` prefixes the rules in
/// messages. A `narrowed` path isn't a built-in global file (its change was
/// confined), and also selects the `changed` rules' targets.
#[allow(clippy::too_many_arguments)]
fn judge_with<'a>(
    config: &Config,
    whose: &str,
    narrowed: bool,
    changed: &[(String, Rule)],
    go: &mut Option<&'a mut dyn Adapter>,
    rust: &mut Option<&'a mut dyn Adapter>,
    path: &str,
    deleted: bool,
) -> PathVerdict {
    let mut v = PathVerdict::default();
    let global = config.is_user_global(path) || (!narrowed && config.is_builtin_global(path));
    if global {
        v.matched_by.insert(MatchKind::Global);
        v.reasons.insert(
            ReasonCode::GlobalFile,
            "every test depends on this file".into(),
        );
    }
    // A detail recorded later replaces an earlier one, so the matching
    // rules' details win over the changed rules'.
    for (at, rule) in changed {
        apply(
            at,
            format!("{at} selects all"),
            &rule.select,
            path,
            go,
            rust,
            &mut v,
        );
    }
    let mut matched = false;
    for (i, rule) in config.rules.iter().enumerate() {
        let Some(glob) = rule.paths.iter().find(|g| g.matches(path)) else {
            continue;
        };
        matched = true;
        let at = format!("{whose}affected.rules[{i}]");
        let all = format!("{at} (`{}`) selects all", glob.as_str());
        apply(&at, all, &rule.select, path, go, rust, &mut v);
    }
    if matched {
        v.matched_by.insert(MatchKind::Rule);
    }
    for adapter in [go.as_deref_mut(), rust.as_deref_mut()]
        .into_iter()
        .flatten()
    {
        match adapter.map(path, deleted) {
            Ok(Mapping::NotMine) => {}
            Ok(m @ (Mapping::Selects(_) | Mapping::Coarse(..))) => {
                let (t, why) = match m {
                    Mapping::Coarse(t, why) => (t, Some(why)),
                    Mapping::Selects(t) => (t, None),
                    _ => unreachable!(),
                };
                v.matched_by.insert(adapter.kind());
                for target in t {
                    if let Target::Rust(package) = &target {
                        let fine = v.selects.iter().any(|t| {
                            matches!(t,
                            Target::RustTest(t) if &t.package == package)
                        });
                        if fine {
                            continue;
                        }
                        if !v.selects.contains(&target) {
                            v.fallback_crates
                                .insert((path.into(), package.clone(), why.clone()));
                        }
                    }
                    v.selects.insert(target);
                }
            }
            Ok(Mapping::Removed(detail)) => {
                v.matched_by.insert(adapter.kind());
                v.reasons.insert(ReasonCode::PackageRemoved, detail);
            }
            Ok(Mapping::Unaccounted(detail)) => {
                v.reasons.insert(ReasonCode::UnmatchedPath, detail);
            }
            Err(e) => {
                v.reasons.insert(
                    ReasonCode::AdapterFailed,
                    format!("the {} adapter failed: {e}", adapter.name()),
                );
            }
        }
    }
    // A narrowed file's tests come only from a rule matching it: an adapter
    // that owns the file (a Go package folder holding a `Cargo.toml`) knows
    // nothing of what a version bump or a rules edit needs.
    if (narrowed && !matched)
        || (v.matched_by.is_empty()
            && !v.reasons.contains_key(&ReasonCode::AdapterFailed)
            && !v.reasons.contains_key(&ReasonCode::UnmatchedPath))
    {
        let none = if whose.is_empty() {
            "no rule matches".to_string()
        } else {
            format!("none of {whose}rules matches")
        };
        let (code, detail) = if !narrowed {
            let when = if whose.is_empty() {
                ""
            } else {
                "; a rule takes effect once the base has it"
            };
            (
                ReasonCode::UnmatchedPath,
                format!("no adapter maps this path and {none} it{when}"),
            )
        } else {
            let what = if path == super::narrow::CONFIG_PATH {
                "the affected rules"
            } else {
                "package versions"
            };
            (
                ReasonCode::GlobalFile,
                format!("only {what} changed, but {none} this file to name its tests"),
            )
        };
        v.reasons.insert(code, detail);
    }
    v
}

/// Applies a matching (or changed) rule's selection to a path's verdict.
/// `at` names the rule in messages; `all` is the detail when it selects
/// everything.
fn apply<'a>(
    at: &str,
    all: String,
    select: &RuleSelect,
    path: &str,
    go: &mut Option<&'a mut dyn Adapter>,
    rust: &mut Option<&'a mut dyn Adapter>,
    v: &mut PathVerdict,
) {
    let (go_names, rust_names, rust_targets, suites) = match select {
        RuleSelect::All => {
            v.reasons.insert(ReasonCode::RuleSelectsAll, all);
            return;
        }
        RuleSelect::None => return,
        RuleSelect::Targets {
            go,
            rust,
            rust_targets,
            suites,
        } => (go, rust, rust_targets, suites),
    };
    v.selects
        .extend(suites.iter().map(|s| Target::Suite(s.clone())));
    for (names, adapter, kind) in [
        (go_names, go.as_deref_mut(), "go"),
        (rust_names, rust.as_deref_mut(), "rust"),
    ] {
        if names.is_empty() {
            continue;
        }
        let resolved = match adapter {
            Some(a) => a.resolve(names),
            None => Err(format!("no {kind} adapter applies to this repository")),
        };
        match resolved {
            Ok(t) => v.selects.extend(t),
            Err(e) => {
                v.reasons.insert(
                    ReasonCode::ConfigInvalid,
                    format!("{at}.select.{kind}: {e}"),
                );
            }
        }
    }
    if rust_targets.is_empty() {
        return;
    }
    let resolved = match rust.as_deref_mut() {
        Some(a) => a.resolve_tests(rust_targets).and_then(|r| {
            if r.missing.is_empty() {
                return Ok((r, None));
            }
            // Every missing target's package must exist at this head: a
            // package the head lacks is an error, even beside targets that
            // resolve.
            let packages: Vec<String> = r
                .missing
                .iter()
                .map(|t| t.package.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let whole = a.resolve(&packages)?;
            // None of the rule's targets exist at this head: run their
            // packages whole.
            let all_missing = r.found.is_empty();
            Ok((r, all_missing.then_some(whole)))
        }),
        None => Err("no rust adapter applies to this repository".into()),
    };
    match resolved {
        Ok((r, whole)) => {
            let all_missing = whole.is_some();
            v.selects.extend(r.found);
            v.selects.extend(whole.into_iter().flatten());
            v.missing_targets.extend(
                r.missing
                    .into_iter()
                    .map(|t| (path.to_string(), at.to_string(), t, all_missing)),
            );
        }
        Err(e) => {
            v.reasons.insert(
                ReasonCode::ConfigInvalid,
                format!("{at}.select.rust_targets: {e}"),
            );
        }
    }
}

/// Groups targets into the output's selection, sorted and deduplicated.
pub fn selection(targets: &BTreeSet<Target>) -> Selected {
    let mut go: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut rust = BTreeSet::new();
    let mut rust_targets = BTreeSet::new();
    let mut suites = BTreeSet::new();
    for t in targets {
        match t {
            Target::Go {
                module,
                dir,
                package,
            } => {
                go.entry((module.clone(), dir.clone()))
                    .or_default()
                    .insert(package.clone());
            }
            Target::Rust(c) => {
                rust.insert(c.clone());
                rust_targets.insert(RustTestSelection::whole(c));
            }
            Target::RustTest(t) => {
                rust.insert(t.package.clone());
                if !targets.contains(&Target::Rust(t.package.clone())) {
                    rust_targets.insert(RustTestSelection::scoped(t));
                }
            }
            Target::Suite(s) => {
                suites.insert(s.clone());
            }
        }
    }
    // Cargo resolves features across selected packages before target filters.
    // Keep that package set together. Target filters apply to every package,
    // so their union may run another package's same-named target too.
    let mut cargo_args = vec!["test".into()];
    for package in &rust {
        cargo_args.extend(["-p".into(), package.clone()]);
    }
    if !rust_targets.iter().any(|t| t.kind == "package") {
        let filters: BTreeSet<Vec<String>> = rust_targets
            .iter()
            .map(|t| t.cargo_args[3..].to_vec())
            .collect();
        cargo_args.extend(filters.into_iter().flatten());
    }
    let rust_targets = rust_targets
        .into_iter()
        .map(|mut target| {
            target.cargo_args = cargo_args.clone();
            target
        })
        .collect();
    Selected {
        go: go
            .into_iter()
            .map(|((module, dir), packages)| GoSelection {
                module,
                dir,
                packages: packages.into_iter().collect(),
            })
            .collect(),
        rust: rust.into_iter().collect(),
        rust_targets,
        suites: suites.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affected::config;

    /// An adapter that owns `*.go` paths and maps each to `pkg:<dir>`.
    struct FakeGo {
        fail: bool,
    }

    impl Adapter for FakeGo {
        fn kind(&self) -> MatchKind {
            MatchKind::Go
        }
        fn name(&self) -> &'static str {
            "go"
        }
        fn map(&mut self, path: &str, deleted: bool) -> Result<Mapping, String> {
            if self.fail {
                return Err("go list failed".into());
            }
            // A package folder holding a non-Go file owns it, as `go`
            // does for files directly in a package's folder.
            if !path.ends_with(".go") && !path.starts_with("owned/") {
                return Ok(Mapping::NotMine);
            }
            if deleted && path.starts_with("gone/") {
                return Ok(Mapping::Removed("gone is gone".into()));
            }
            let dir = path.rsplit_once('/').map_or(".", |(d, _)| d);
            Ok(Mapping::Selects(BTreeSet::from([go(dir)])))
        }
        fn resolve(&mut self, names: &[String]) -> Result<BTreeSet<Target>, String> {
            names
                .iter()
                .map(|n| match n.strip_prefix("./") {
                    Some(d) if d != "missing" => Ok(go(d)),
                    _ => Err(format!("no package matches {n}")),
                })
                .collect()
        }
    }

    fn go(dir: &str) -> Target {
        Target::Go {
            module: "m".into(),
            dir: ".".into(),
            package: format!("m/{dir}"),
        }
    }

    fn cfg(json: &str) -> Config {
        config::parse(json).unwrap()
    }

    fn judge(
        config: &Config,
        adapter: Option<&mut FakeGo>,
        path: &str,
        deleted: bool,
    ) -> PathVerdict {
        let mut e = Engine {
            config,
            go: adapter.map(|a| a as &mut dyn Adapter),
            rust: None,
            narrowing: Narrowing::default(),
        };
        e.judge(path, deleted)
    }

    fn codes(v: &PathVerdict) -> Vec<ReasonCode> {
        v.reasons.keys().copied().collect()
    }

    #[test]
    fn an_unmatched_path_is_full() {
        let c = cfg("{}");
        let v = judge(&c, Some(&mut FakeGo { fail: false }), "scripts/x.sh", false);
        assert_eq!(codes(&v), [ReasonCode::UnmatchedPath]);
        assert!(v.matched_by.is_empty());
    }

    #[test]
    fn an_adapter_path_selects_its_targets() {
        let c = cfg("{}");
        let v = judge(
            &c,
            Some(&mut FakeGo { fail: false }),
            "internal/x/a.go",
            false,
        );
        assert!(codes(&v).is_empty());
        assert_eq!(v.selects, BTreeSet::from([go("internal/x")]));
        assert_eq!(v.matched_by, BTreeSet::from([MatchKind::Go]));
    }

    #[test]
    fn a_global_file_is_full_even_when_an_adapter_maps_it() {
        let c = cfg(r#"{"affected": {"global": ["internal/testutil/**"]}}"#);
        let v = judge(
            &c,
            Some(&mut FakeGo { fail: false }),
            "internal/testutil/h.go",
            false,
        );
        assert_eq!(codes(&v), [ReasonCode::GlobalFile]);
        let v = judge(&c, None, "go.mod", false);
        assert_eq!(codes(&v), [ReasonCode::GlobalFile]);
    }

    #[test]
    fn rules_select_all_none_or_targets() {
        let c = cfg(r#"{"affected": {"rules": [
                {"paths": ["api/**"], "select": "all"},
                {"paths": ["docs/**"], "select": "none"},
                {"paths": ["migrations/**"], "select": {"go": ["./internal/repo"], "suites": ["db"]}}]}}"#);
        let mut a = FakeGo { fail: false };
        assert_eq!(
            codes(&judge(&c, Some(&mut a), "api/v1/x.proto", false)),
            [ReasonCode::RuleSelectsAll]
        );
        let none = judge(&c, Some(&mut a), "docs/guide.md", false);
        assert!(codes(&none).is_empty(), "a none rule accounts for the path");
        assert!(none.selects.is_empty());
        let t = judge(&c, Some(&mut a), "migrations/001.sql", false);
        assert!(codes(&t).is_empty());
        assert_eq!(
            t.selects,
            BTreeSet::from([go("internal/repo"), Target::Suite("db".into())])
        );
    }

    #[test]
    fn a_none_rule_doesnt_hide_an_adapters_selection() {
        let c = cfg(r#"{"affected": {"rules": [{"paths": ["**/*.go"], "select": "none"}]}}"#);
        let v = judge(
            &c,
            Some(&mut FakeGo { fail: false }),
            "internal/x/a.go",
            false,
        );
        assert_eq!(v.selects, BTreeSet::from([go("internal/x")]));
    }

    #[test]
    fn a_rule_target_nothing_resolves_is_full() {
        let c = cfg(
            r#"{"affected": {"rules": [{"paths": ["m/**"], "select": {"go": ["./missing"]}}]}}"#,
        );
        let v = judge(&c, Some(&mut FakeGo { fail: false }), "m/a.sql", false);
        assert_eq!(codes(&v), [ReasonCode::ConfigInvalid]);
        let v = judge(&c, None, "m/a.sql", false);
        assert_eq!(
            codes(&v),
            [ReasonCode::ConfigInvalid],
            "no go adapter at all"
        );
    }

    #[test]
    fn an_adapter_failure_is_full() {
        let c = cfg("{}");
        let v = judge(
            &c,
            Some(&mut FakeGo { fail: true }),
            "internal/x/a.go",
            false,
        );
        assert_eq!(codes(&v), [ReasonCode::AdapterFailed]);
    }

    #[test]
    fn a_removed_package_is_full() {
        let c = cfg("{}");
        let v = judge(&c, Some(&mut FakeGo { fail: false }), "gone/a.go", true);
        assert_eq!(codes(&v), [ReasonCode::PackageRemoved]);
    }

    #[test]
    fn a_narrowed_file_an_adapter_owns_still_needs_a_rule() {
        let narrowing = || Narrowing {
            paths: BTreeMap::from([("owned/Cargo.toml".to_string(), Vec::new())]),
            base: None,
        };
        let judge = |c: &Config| {
            let mut a = FakeGo { fail: false };
            let mut e = Engine {
                config: c,
                go: Some(&mut a as &mut dyn Adapter),
                rust: None,
                narrowing: narrowing(),
            };
            e.judge("owned/Cargo.toml", false)
        };
        let v = judge(&cfg("{}"));
        assert_eq!(codes(&v), [ReasonCode::GlobalFile], "{v:?}");
        assert!(v.reasons[&ReasonCode::GlobalFile].contains("no rule matches"));
        let c = cfg(
            r#"{"affected": {"rules": [{"paths": ["owned/Cargo.toml"], "select": {"suites": ["version"]}}]}}"#,
        );
        let v = judge(&c);
        assert!(codes(&v).is_empty(), "{v:?}");
        assert!(v.selects.contains(&Target::Suite("version".into())));
        // Not narrowed, the same file is still global.
        let mut a = FakeGo { fail: false };
        let v = judge_one(&c, &mut a, "owned/Cargo.toml");
        assert_eq!(codes(&v), [ReasonCode::GlobalFile]);
    }

    fn judge_one(c: &Config, a: &mut FakeGo, path: &str) -> PathVerdict {
        judge(c, Some(a), path, false)
    }

    #[test]
    fn the_selection_groups_and_sorts() {
        let s = selection(&BTreeSet::from([
            go("b"),
            go("a"),
            Target::Rust("z".into()),
            Target::Rust("a".into()),
            Target::Suite("s".into()),
        ]));
        assert_eq!(s.go.len(), 1);
        assert_eq!(s.go[0].packages, ["m/a", "m/b"]);
        assert_eq!(s.rust, ["a", "z"]);
        assert_eq!(s.suites, ["s"]);
    }
}
