//! The selection engine: each changed path goes through the global files,
//! the config rules and the language adapters, and anything none of them
//! accounts for forces a full run.

use std::collections::{BTreeMap, BTreeSet};

use super::config::{Config, RuleSelect};
use super::{GoSelection, MatchKind, ReasonCode, Selected};

/// One test target.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Go {
        module: String,
        dir: String,
        package: String,
    },
    Rust(String),
    Suite(String),
}

impl Target {
    /// How `paths[].selects` names it.
    pub fn label(&self) -> String {
        match self {
            Target::Go { package, .. } => format!("go:{package}"),
            Target::Rust(c) => format!("rust:{c}"),
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
}

/// The engine's verdict for one path.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PathVerdict {
    pub matched_by: BTreeSet<MatchKind>,
    pub selects: BTreeSet<Target>,
    /// The reasons it forces a full run, with detail.
    pub reasons: BTreeMap<ReasonCode, String>,
}

pub struct Engine<'a> {
    pub config: &'a Config,
    pub go: Option<&'a mut dyn Adapter>,
    pub rust: Option<&'a mut dyn Adapter>,
}

impl Engine<'_> {
    /// Judges one path: `deleted` when it doesn't exist at the head.
    pub fn judge(&mut self, path: &str, deleted: bool) -> PathVerdict {
        let mut v = PathVerdict::default();
        if self.config.is_global(path) {
            v.matched_by.insert(MatchKind::Global);
            v.reasons.insert(
                ReasonCode::GlobalFile,
                "every test depends on this file".into(),
            );
        }
        for (i, rule) in self.config.rules.iter().enumerate() {
            let Some(glob) = rule.paths.iter().find(|g| g.matches(path)) else {
                continue;
            };
            v.matched_by.insert(MatchKind::Rule);
            match &rule.select {
                RuleSelect::All => {
                    v.reasons.insert(
                        ReasonCode::RuleSelectsAll,
                        format!("affected.rules[{i}] (`{}`) selects all", glob.as_str()),
                    );
                }
                RuleSelect::None => {}
                RuleSelect::Targets { go, rust, suites } => {
                    v.selects
                        .extend(suites.iter().map(|s| Target::Suite(s.clone())));
                    for (names, adapter, kind) in [
                        (go, self.go.as_deref_mut(), "go"),
                        (rust, self.rust.as_deref_mut(), "rust"),
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
                                    format!("affected.rules[{i}].select.{kind}: {e}"),
                                );
                            }
                        }
                    }
                }
            }
        }
        for adapter in [self.go.as_deref_mut(), self.rust.as_deref_mut()]
            .into_iter()
            .flatten()
        {
            match adapter.map(path, deleted) {
                Ok(Mapping::NotMine) => {}
                Ok(Mapping::Selects(t)) => {
                    v.matched_by.insert(adapter.kind());
                    v.selects.extend(t);
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
        if v.matched_by.is_empty()
            && !v.reasons.contains_key(&ReasonCode::AdapterFailed)
            && !v.reasons.contains_key(&ReasonCode::UnmatchedPath)
        {
            v.reasons.insert(
                ReasonCode::UnmatchedPath,
                "no adapter maps this path and no rule matches it".into(),
            );
        }
        v
    }
}

/// Groups targets into the output's selection, sorted and deduplicated.
pub fn selection(targets: &BTreeSet<Target>) -> Selected {
    let mut go: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut rust = BTreeSet::new();
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
            }
            Target::Suite(s) => {
                suites.insert(s.clone());
            }
        }
    }
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
            if !path.ends_with(".go") {
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
