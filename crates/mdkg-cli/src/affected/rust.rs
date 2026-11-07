//! The Rust adapter: maps changed paths to the workspace crates whose tests
//! need to run, from `cargo metadata --no-deps --offline`, which reads the
//! manifests only and needs no registry.
//!
//! - A file maps to a member when it lies in the folder holding one of the
//!   member's targets (`src/`, `tests/`, `src/bin/`); when the target sits
//!   at the member's top (`build.rs`, a top-level `lib.rs`), the `.rs`
//!   files beside it; when it sits outside the member, the `.rs` files
//!   under its folder (its modules), or only those beside it at the
//!   repository's top. A path several members claim selects them all. A
//!   target whose file is missing at the head makes the run full. Anything else in a member's folder (a README, a
//!   `fixtures/` folder another crate's tests read) is left to the rules,
//!   so a crate at the repository's top doesn't claim every path.
//! - A crate's dependents are the members that depend on it through a
//!   `path` dependency, transitively for normal and build dependencies,
//!   plus every member that has it, or one of those, as a dev-dependency.
//!   Dev-dependencies end the walk, as test imports do in Go.
//! - Files outside every member (the workspace `Cargo.toml` and
//!   `Cargo.lock` are global files anyway) are left to the rules.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::MatchKind;
use super::config::{RustTarget, RustTargetKind};
use super::modules;
use super::select::{Adapter, Mapping, Target, TestResolution};

#[derive(Debug, Deserialize)]
struct Metadata {
    packages: Vec<MetaPackage>,
    workspace_members: Vec<String>,
    workspace_root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct MetaPackage {
    id: String,
    name: String,
    manifest_path: PathBuf,
    #[serde(default)]
    dependencies: Vec<MetaDependency>,
    #[serde(default)]
    targets: Vec<MetaTarget>,
}

#[derive(Debug, Deserialize)]
struct MetaTarget {
    src_path: PathBuf,
    name: String,
    kind: Vec<String>,
    test: bool,
    #[serde(default, rename = "required-features")]
    required_features: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct MetaDependency {
    /// `null` for a normal dependency, `dev` or `build`.
    kind: Option<String>,
    path: Option<PathBuf>,
}

/// One target's crate root, for reading its module tree.
#[derive(Debug, Clone)]
struct TargetFile {
    /// What a rule or a derived selection names it by; `None` for a
    /// target `cargo test` builds but that has no test scope here (an
    /// example), or doesn't build (a bench).
    scope: Option<RustTarget>,
    /// Its crate root, repository-relative.
    src: String,
}

#[derive(Debug, Default)]
struct Graph {
    test_targets: BTreeSet<RustTarget>,
    /// Package to its targets' crate roots.
    roots: BTreeMap<String, Vec<TargetFile>>,
    unsupported_targets: BTreeMap<RustTarget, &'static str>,
    /// Repository-relative member folder to crate name.
    by_dir: BTreeMap<String, String>,
    /// What members claim, to the crates claiming it.
    claims: BTreeMap<Claim, BTreeSet<String>>,
    /// Crate to the members depending on it (normal or build).
    importers: BTreeMap<String, BTreeSet<String>>,
    /// Crate to the members having it as a dev-dependency.
    dev_importers: BTreeMap<String, BTreeSet<String>>,
}

/// A set of paths a member claims.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Claim {
    /// Everything under a folder (`.` is the repository's top).
    Folder(String),
    /// The `.rs` files directly in a folder.
    TopRs(String),
    /// The `.rs` files anywhere under a folder: a target outside its
    /// member. Its other files can be read by that target (`include_str!`)
    /// whatever else claims them, so a change to one is full.
    RsUnder(String),
    /// The `.rs` files directly in the repository's top folder, for a
    /// target outside its member that sits there; the other files there
    /// are full, as for RsUnder.
    OutsideTopRs,
}

impl Claim {
    fn holds(&self, path: &str) -> bool {
        match self {
            Claim::Folder(dir) => dir == "." || within(dir, path),
            Claim::RsUnder(dir) => within(dir, path) && path.ends_with(".rs"),
            Claim::OutsideTopRs => !path.contains('/') && path.ends_with(".rs"),
            Claim::TopRs(dir) => {
                let parent = path.rsplit_once('/').map_or(".", |(d, _)| d);
                parent == dir && path.ends_with(".rs")
            }
        }
    }
}

pub struct RustAdapter {
    top: PathBuf,
    /// An empty private folder for cargo's home.
    cache: PathBuf,
    /// Every committed manifest, checked before Cargo can follow its paths.
    manifests: Vec<String>,
    graph: Option<Result<Graph, String>>,
    /// Package to its targets' module trees, read when first needed.
    trees: BTreeMap<String, Vec<(TargetFile, modules::Tree)>>,
    /// The head's tracked paths, which module trees match exactly.
    tracked: BTreeSet<String>,
}

fn within(dir: &str, path: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// One filename predicate for snapshot and adapter discovery, including hosts
/// with case-insensitive paths.
pub fn is_manifest_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("Cargo.toml")
}

impl RustAdapter {
    /// Include case variants so preflight can reject ambiguous manifests on
    /// every host before Cargo follows them on a case-insensitive filesystem.
    pub fn discover(top: &Path, cache: &Path, tracked: &[String]) -> Option<Self> {
        tracked.iter().any(|p| is_manifest_name(p)).then(|| Self {
            top: top.to_path_buf(),
            cache: cache.to_path_buf(),
            manifests: tracked
                .iter()
                .filter(|p| p.rsplit('/').next().is_some_and(is_manifest_name))
                .cloned()
                .collect(),
            graph: None,
            trees: BTreeMap::new(),
            tracked: tracked.iter().cloned().collect(),
        })
    }

    /// Rule validation happens before changed paths are judged, including
    /// global-only and empty diffs. Legacy package commands omit these scopes.
    pub fn unsupported_rule_target(
        &mut self,
        targets: &[RustTarget],
    ) -> Result<Option<(RustTarget, &'static str)>, String> {
        let graph = self.graph()?;
        Ok(targets.iter().find_map(|t| {
            graph
                .unsupported_targets
                .get(t)
                .map(|reason| (t.clone(), *reason))
        }))
    }

    /// The test targets a `.rs` file needs, read from the module trees of
    /// every workspace target: each target whose tree reaches it, when all
    /// of them are integration tests. A file only test targets reach is
    /// part of no library or binary, so no dependent needs testing. `Err`
    /// says why whole crates are needed instead, with the packages whose
    /// other targets reach the file (beside its owners).
    fn derive(
        &mut self,
        path: &str,
        deleted: bool,
    ) -> Result<BTreeSet<Target>, (String, BTreeSet<String>)> {
        let graph = self.graph().map_err(|e| (e, BTreeSet::new()))?;
        let unsupported = graph.unsupported_targets.clone();
        let packages: Vec<String> = graph.roots.keys().cloned().collect();
        let mut out = BTreeSet::new();
        let mut coarse: Option<String> = None;
        let mut reaching = BTreeSet::new();
        for package in packages {
            for (target, tree) in self.trees_of(&package) {
                // An uncertain tree may reach any file: a computed include,
                // `#[path]` or another spelling can name any tracked path,
                // in or out of its root's folder.
                let maybe = tree.uncertain.is_some();
                if !tree.reaches(path) && !maybe {
                    continue;
                }
                match &target.scope {
                    Some(
                        scope @ RustTarget {
                            kind: RustTargetKind::Test(_),
                            ..
                        },
                    ) if !unsupported.contains_key(scope) => {
                        out.insert(Target::RustTest(scope.clone()));
                    }
                    scope => {
                        reaching.insert(package.clone());
                        if coarse.is_none() {
                            let what = match scope {
                                Some(RustTarget {
                                    kind: RustTargetKind::Lib,
                                    ..
                                }) => format!("{package}'s library"),
                                Some(RustTarget {
                                    kind: RustTargetKind::Bin(name),
                                    ..
                                }) => format!("{package}'s binary {name}"),
                                Some(RustTarget {
                                    kind: RustTargetKind::Test(name),
                                    ..
                                }) => format!("{package}'s opt-in test {name}"),
                                None => format!("{package}'s target {}", target.src),
                            };
                            coarse = Some(match &tree.uncertain {
                                Some(why) if !tree.reaches(path) => {
                                    format!("{what} may reach it ({why})")
                                }
                                _ => format!("{what} reaches it"),
                            });
                        }
                    }
                }
            }
        }
        let late = if !path.ends_with(".rs") {
            Some("it isn't a .rs file, so a test may read it at run time")
        } else if deleted {
            Some("it is deleted, so the head can't show what reached it")
        } else if out.is_empty() {
            Some("no target's module tree reaches it, so a test may read it at run time")
        } else {
            None
        };
        if let Some(why) = coarse.or(late.map(String::from)) {
            // Whatever reaches it runs whole too: an include_str! from
            // another crate's tests reads this file.
            reaching.extend(out.iter().filter_map(|t| match t {
                Target::RustTest(t) => Some(t.package.clone()),
                _ => None,
            }));
            return Err((why, reaching));
        }
        Ok(out)
    }

    /// A package's targets' module trees, read once.
    fn trees_of(&mut self, package: &str) -> &Vec<(TargetFile, modules::Tree)> {
        if !self.trees.contains_key(package) {
            let roots = match &self.graph {
                Some(Ok(g)) => g.roots.get(package).cloned().unwrap_or_default(),
                _ => Vec::new(),
            };
            let trees = roots
                .into_iter()
                .map(|t| {
                    let tree = modules::scan(&self.top, &t.src, &self.tracked);
                    (t, tree)
                })
                .collect();
            self.trees.insert(package.to_string(), trees);
        }
        &self.trees[package]
    }

    fn graph(&mut self) -> Result<&Graph, String> {
        if self.graph.is_none() {
            self.graph = Some(load(&self.top, &self.cache, &self.manifests));
        }
        match self.graph.as_ref() {
            Some(Ok(g)) => Ok(g),
            Some(Err(e)) => Err(e.clone()),
            None => Err("the Rust graph didn't load".into()),
        }
    }
}

fn relative(top: &Path, dir: &Path) -> Result<String, String> {
    // cargo metadata doesn't check that a target's file exists, nor resolve
    // `..`: a path that can't be resolved in the head's tree is an error,
    // and the run is full.
    let dir = std::fs::canonicalize(dir).map_err(|e| {
        format!(
            "cargo metadata named {}, which the head doesn't have: {e}",
            dir.display()
        )
    })?;
    let rel = dir.strip_prefix(top).map_err(|_| {
        format!(
            "cargo metadata reported {} outside the repository",
            dir.display()
        )
    })?;
    let text = rel
        .to_str()
        .ok_or_else(|| format!("{} isn't UTF-8", rel.display()))?
        .replace('\\', "/");
    Ok(if text.is_empty() { ".".into() } else { text })
}

/// A manifest path must stay within the committed snapshot. Check lexically
/// before Cargo (or canonicalize) can open an outside manifest. Snapshot trees
/// contain no symlinks, so an in-tree lexical path cannot redirect elsewhere.
fn confined(manifest: &str, value: &str) -> Result<(), String> {
    let mut depth = manifest.split('/').count() - 1;
    let mut glob = false;
    for part in Path::new(value).components() {
        match part {
            Component::Normal(segment) => {
                // In workspace membership globs, ** may match zero folders.
                // Don't count it as a guaranteed level before a later `..`.
                glob |= segment.to_string_lossy().contains(['*', '?', '[']);
                depth += 1;
            }
            Component::CurDir => {}
            Component::ParentDir if depth > 0 && !glob => depth -= 1,
            _ => {
                return Err(format!(
                    "{manifest} names a path outside the committed snapshot: {value}"
                ));
            }
        }
    }
    Ok(())
}

fn check_path_item(manifest: &str, item: &toml_edit::Item) -> Result<(), String> {
    let value = item
        .as_str()
        .ok_or_else(|| format!("{manifest} has a non-string path"))?;
    confined(manifest, value)
}

fn dependency_paths(manifest: &str, table: &dyn toml_edit::TableLike) -> Result<(), String> {
    // Cargo accepts the underscore aliases in pre-2024 editions. Check both
    // spellings even if both appear, including target-specific dependencies.
    for key in [
        "dependencies",
        "dev-dependencies",
        "build-dependencies",
        "dev_dependencies",
        "build_dependencies",
    ] {
        if let Some(deps) = table.get(key).and_then(toml_edit::Item::as_table_like) {
            for (_, dep) in deps.iter() {
                if let Some(path) = dep.as_table_like().and_then(|d| d.get("path")) {
                    check_path_item(manifest, path)?;
                }
            }
        }
    }
    Ok(())
}

/// Decode TOML keys instead of searching raw text: quoted Unicode escapes,
/// dotted keys and inline tables must not bypass patch or toolchain guards.
fn redirections(top: &Path, manifests: &[String]) -> Result<(), String> {
    for config in [".cargo/config", ".cargo/config.toml"] {
        if top.join(config).exists() {
            return Err(format!(
                "{config} can patch or redirect dependencies, which cargo metadata --no-deps doesn't show"
            ));
        }
    }
    for file in ["rust-toolchain", "rust-toolchain.toml"] {
        let text = match std::fs::read_to_string(top.join(file)) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("can't read {file}: {e}")),
        };
        // rustup also accepts a single legacy channel name in rust-toolchain.
        let channel = text.trim();
        if file == "rust-toolchain"
            && !channel.is_empty()
            && channel
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            continue;
        }
        let doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("can't parse {file}: {e}"))?;
        let toolchain = doc
            .get("toolchain")
            .and_then(toml_edit::Item::as_table_like)
            .ok_or_else(|| format!("{file} has no toolchain table"))?;
        if toolchain.contains_key("path") {
            return Err(format!(
                "{file} names a toolchain by path, which rustup would run"
            ));
        }
    }
    for manifest in manifests {
        if manifest.rsplit('/').next() != Some("Cargo.toml") {
            return Err(format!(
                "{manifest} has noncanonical casing; Cargo may open it as Cargo.toml on a case-insensitive filesystem"
            ));
        }
        let text = std::fs::read_to_string(top.join(manifest))
            .map_err(|e| format!("can't read {manifest}: {e}"))?;
        let doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("can't parse {manifest}: {e}"))?;
        if doc.contains_key("patch") || doc.contains_key("replace") {
            return Err(format!(
                "{manifest} may patch or replace dependencies, which cargo metadata --no-deps doesn't show"
            ));
        }
        dependency_paths(manifest, doc.as_table())?;
        if let Some(workspace) = doc
            .get("workspace")
            .and_then(toml_edit::Item::as_table_like)
        {
            dependency_paths(manifest, workspace)?;
            for key in ["members", "default-members", "exclude"] {
                if let Some(paths) = workspace.get(key).and_then(toml_edit::Item::as_array) {
                    for path in paths {
                        let path = path
                            .as_str()
                            .ok_or_else(|| format!("{manifest} has a non-string workspace path"))?;
                        confined(manifest, path)?;
                    }
                }
            }
        }
        if let Some(targets) = doc.get("target").and_then(toml_edit::Item::as_table_like) {
            for (_, target) in targets.iter() {
                if let Some(target) = target.as_table_like() {
                    dependency_paths(manifest, target)?;
                }
            }
        }
        // `project` is Cargo's legacy spelling of `package`.
        for key in ["package", "project"] {
            if let Some(package) = doc.get(key).and_then(toml_edit::Item::as_table_like) {
                if let Some(path) = package.get("workspace") {
                    check_path_item(manifest, path)?;
                }
                if let Some(path) = package.get("build").filter(|p| p.as_str().is_some()) {
                    check_path_item(manifest, path)?;
                }
            }
        }
        if let Some(path) = doc
            .get("lib")
            .and_then(toml_edit::Item::as_table_like)
            .and_then(|lib| lib.get("path"))
        {
            check_path_item(manifest, path)?;
        }
        for kind in ["bin", "test", "bench", "example"] {
            if let Some(targets) = doc.get(kind).and_then(toml_edit::Item::as_array_of_tables) {
                for target in targets {
                    if let Some(path) = target.get("path") {
                        check_path_item(manifest, path)?;
                    }
                }
            } else if let Some(targets) = doc.get(kind).and_then(toml_edit::Item::as_array) {
                for target in targets {
                    if let Some(path) = target.as_inline_table().and_then(|t| t.get("path")) {
                        let path = path
                            .as_str()
                            .ok_or_else(|| format!("{manifest} has a non-string target path"))?;
                        confined(manifest, path)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn load(top: &Path, cache: &Path, manifests: &[String]) -> Result<Graph, String> {
    redirections(top, manifests)?;
    let out = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .current_dir(top)
        // A toolchain the head pins but this machine lacks fails the load
        // (full) instead of being downloaded by rustup.
        .env("RUSTUP_AUTO_INSTALL", "0")
        // No cargo config of the caller's (in their cargo home) patches
        // dependencies either.
        .env("CARGO_HOME", cache.join("cargo-home"))
        .output()
        .map_err(|e| format!("can't run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let meta: Metadata = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("cargo metadata output is unreadable: {e}"))?;
    from_metadata(top, meta)
}

fn from_metadata(top: &Path, meta: Metadata) -> Result<Graph, String> {
    // The workspace's own manifest is the one checked for patches: one
    // elsewhere (`workspace = "..."`) isn't.
    if relative(top, &meta.workspace_root)? != "." {
        return Err(format!(
            "the workspace root is {}, not the repository's top",
            meta.workspace_root.display()
        ));
    }
    let members: BTreeSet<&str> = meta.workspace_members.iter().map(String::as_str).collect();
    let mut graph = Graph::default();
    // Canonical member folder to crate name, to resolve path dependencies.
    let mut by_abs: BTreeMap<PathBuf, String> = BTreeMap::new();
    for p in meta
        .packages
        .iter()
        .filter(|p| members.contains(p.id.as_str()))
    {
        let dir = p
            .manifest_path
            .parent()
            .ok_or_else(|| format!("{} has no folder", p.manifest_path.display()))?;
        let rel = relative(top, dir)?;
        if let Some(previous) = graph.by_dir.insert(rel.clone(), p.name.clone()) {
            return Err(format!("two crates in {rel}: {previous} and {}", p.name));
        }
        for target in &p.targets {
            let kind = if target.kind.iter().any(|k| k == "test") {
                Some(RustTargetKind::Test(target.name.clone()))
            } else if target.kind.iter().any(|k| {
                matches!(
                    k.as_str(),
                    "lib" | "rlib" | "dylib" | "staticlib" | "cdylib" | "proc-macro"
                )
            }) {
                Some(RustTargetKind::Lib)
            } else if target.kind.iter().any(|k| k == "bin") {
                Some(RustTargetKind::Bin(target.name.clone()))
            } else {
                None
            };
            let target_scope = kind.clone().map(|kind| RustTarget {
                package: p.name.clone(),
                kind,
            });
            if let Some(kind) = kind {
                let scope = RustTarget {
                    package: p.name.clone(),
                    kind,
                };
                graph.test_targets.insert(scope.clone());
                let reason = if !target.test {
                    Some("test = false")
                } else if !target.required_features.is_empty() {
                    Some("required-features")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    graph.unsupported_targets.insert(scope, reason);
                }
            }
            let src = relative(top, &target.src_path)?;
            graph
                .roots
                .entry(p.name.clone())
                .or_default()
                .push(TargetFile {
                    scope: target_scope.clone(),
                    src: src.clone(),
                });
            let inside = if rel == "." {
                Some(src.as_str())
            } else {
                src.strip_prefix(&rel).and_then(|r| r.strip_prefix('/'))
            };
            let folder_of = |path: &str| {
                path.rsplit_once('/')
                    .map_or(".".to_string(), |(d, _)| d.to_string())
            };
            let claim = match inside {
                // Under the member, in a folder: that folder.
                Some(r) if r.contains('/') => Claim::Folder(folder_of(&src)),
                // At the member's top: the file, and the `.rs` files beside
                // it, which its `mod` lines can name.
                Some(_) => Claim::TopRs(rel.clone()),
                // Outside the member: the folder holding the file, whose
                // modules it pulls in; at the repository's top, only the
                // `.rs` files there, never every path.
                None => match folder_of(&src) {
                    top if top == "." => Claim::OutsideTopRs,
                    folder => Claim::RsUnder(folder),
                },
            };
            graph
                .claims
                .entry(claim)
                .or_default()
                .insert(p.name.clone());
        }
        by_abs.insert(
            std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()),
            p.name.clone(),
        );
    }
    for p in meta
        .packages
        .iter()
        .filter(|p| members.contains(p.id.as_str()))
    {
        for d in &p.dependencies {
            let Some(path) = &d.path else { continue };
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            let Some(dep) = by_abs.get(&canonical) else {
                // A nonmember, inside or outside the repository, can depend
                // back on members; --no-deps doesn't include those edges.
                return Err(format!(
                    "{} depends on {}, a crate that isn't a workspace member",
                    p.name,
                    canonical.display()
                ));
            };
            let edges = if d.kind.as_deref() == Some("dev") {
                &mut graph.dev_importers
            } else {
                &mut graph.importers
            };
            edges.entry(dep.clone()).or_default().insert(p.name.clone());
        }
    }
    Ok(graph)
}

impl Graph {
    fn dependents(&self, seed: &str) -> BTreeSet<Target> {
        let mut closure = BTreeSet::from([seed.to_string()]);
        let mut queue = VecDeque::from([seed.to_string()]);
        while let Some(c) = queue.pop_front() {
            for importer in self.importers.get(&c).into_iter().flatten() {
                if closure.insert(importer.clone()) {
                    queue.push_back(importer.clone());
                }
            }
        }
        let mut tested = closure.clone();
        for c in &closure {
            tested.extend(self.dev_importers.get(c).into_iter().flatten().cloned());
        }
        tested.into_iter().map(Target::Rust).collect()
    }

    /// Every member with a claim on `path`.
    fn owners(&self, path: &str) -> BTreeSet<String> {
        self.claims
            .iter()
            .filter(|(claim, _)| claim.holds(path))
            .flat_map(|(_, names)| names.iter().cloned())
            .collect()
    }
}

impl Adapter for RustAdapter {
    fn kind(&self) -> MatchKind {
        MatchKind::Rust
    }

    fn name(&self) -> &'static str {
        "rust"
    }

    fn map(&mut self, path: &str, deleted: bool) -> Result<Mapping, String> {
        let graph = self.graph()?;
        if !path.ends_with(".rs")
            && graph.claims.keys().any(|c| match c {
                Claim::RsUnder(d) => within(d, path),
                Claim::OutsideTopRs => !path.contains('/'),
                _ => false,
            })
        {
            return Ok(Mapping::Unaccounted(
                "a target outside its crate lives beside or above this file, and can read it"
                    .into(),
            ));
        }
        let owners = graph.owners(path);
        if owners.is_empty() {
            return Ok(Mapping::NotMine);
        }
        Ok(match self.derive(path, deleted) {
            Ok(fine) => Mapping::Selects(fine),
            Err((why, reaching)) => {
                let graph = self.graph()?;
                Mapping::Coarse(
                    owners
                        .iter()
                        .chain(&reaching)
                        .flat_map(|o| graph.dependents(o))
                        .collect(),
                    why,
                )
            }
        })
    }

    fn resolve(&mut self, names: &[String]) -> Result<BTreeSet<Target>, String> {
        let graph = self.graph()?;
        let known: BTreeSet<&String> = graph.by_dir.values().collect();
        names
            .iter()
            .map(|n| {
                if known.contains(n) {
                    Ok(Target::Rust(n.clone()))
                } else {
                    Err(format!("no workspace crate is named `{n}`"))
                }
            })
            .collect()
    }

    fn resolve_tests(&mut self, targets: &[RustTarget]) -> Result<TestResolution, String> {
        let graph = self.graph()?;
        let mut out = TestResolution::default();
        for target in targets {
            if graph.test_targets.contains(target) {
                out.found.insert(Target::RustTest(target.clone()));
            } else {
                out.missing.push(target.clone());
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Graph {
        // core <- graph <- cli; cli dev-depends on fixtures; fixtures depends
        // on core; build depends on core as a build-dependency.
        let mut g = Graph::default();
        for (dir, name) in [
            ("crates/core", "core"),
            ("crates/graph", "graph"),
            ("crates/cli", "cli"),
            ("crates/fixtures", "fixtures"),
            ("crates/build", "build"),
        ] {
            member(&mut g, dir, name);
        }
        let edge = |m: &mut BTreeMap<String, BTreeSet<String>>, dep: &str, by: &str| {
            m.entry(dep.into()).or_default().insert(by.into());
        };
        edge(&mut g.importers, "core", "graph");
        edge(&mut g.importers, "graph", "cli");
        edge(&mut g.importers, "core", "fixtures");
        edge(&mut g.importers, "core", "build");
        edge(&mut g.dev_importers, "fixtures", "cli");
        g
    }

    /// A member at `dir` with the usual `src/` and `tests/` targets.
    fn member(g: &mut Graph, dir: &str, name: &str) {
        g.by_dir.insert(dir.into(), name.into());
        for folder in ["src", "tests"] {
            let claim = if dir == "." {
                folder.to_string()
            } else {
                format!("{dir}/{folder}")
            };
            g.claims
                .entry(Claim::Folder(claim))
                .or_default()
                .insert(name.into());
        }
        // build.rs at the top.
        g.claims
            .entry(Claim::TopRs(dir.into()))
            .or_default()
            .insert(name.into());
    }

    fn names(t: &BTreeSet<Target>) -> Vec<String> {
        t.iter().map(Target::label).collect()
    }

    #[test]
    fn dependents_follow_normal_and_build_edges_and_stop_at_dev_edges() {
        let g = graph();
        assert_eq!(
            names(&g.dependents("core")),
            [
                "rust:build",
                "rust:cli",
                "rust:core",
                "rust:fixtures",
                "rust:graph"
            ]
        );
        assert_eq!(
            names(&g.dependents("fixtures")),
            ["rust:cli", "rust:fixtures"]
        );
        assert_eq!(names(&g.dependents("cli")), ["rust:cli"]);
    }

    #[test]
    fn a_path_belongs_to_every_member_claiming_it() {
        let mut g = graph();
        member(&mut g, "crates/core/src/inner", "inner");
        let owners = |g: &Graph, p: &str| g.owners(p).into_iter().collect::<Vec<_>>();
        assert_eq!(owners(&g, "crates/core/src/lib.rs"), ["core"]);
        assert_eq!(owners(&g, "crates/core/tests/it.rs"), ["core"]);
        assert_eq!(owners(&g, "crates/core/build.rs"), ["core"]);
        // Nested: both the outer crate's src/ and the inner crate claim it.
        assert_eq!(
            owners(&g, "crates/core/src/inner/src/lib.rs"),
            ["core", "inner"]
        );
        // Outside every target folder: the rules decide.
        assert!(owners(&g, "crates/core/README.md").is_empty());
        assert!(owners(&g, "crates/core/fixtures/data.json").is_empty());
        assert!(owners(&g, "crates/coreX/src/lib.rs").is_empty());
        // A crate at the top claims only its own target folders.
        member(&mut g, ".", "root");
        assert_eq!(owners(&g, "src/main.rs"), ["root"]);
        assert!(owners(&g, "scripts/check.sh").is_empty());
        assert!(owners(&g, "fixtures/data.json").is_empty());
        assert_eq!(owners(&g, "crates/cli/src/main.rs"), ["cli"]);
        // Two crates sharing a folder outside them both claim it.
        for name in ["b", "c"] {
            g.claims
                .entry(Claim::Folder("conformance".into()))
                .or_default()
                .insert(name.into());
        }
        assert_eq!(owners(&g, "conformance/run.rs"), ["b", "c"]);
        assert_eq!(owners(&g, "conformance/common/mod.rs"), ["b", "c"]);
        // A top-level target's sibling .rs files, but not its folder's
        // other files.
        assert_eq!(owners(&g, "crates/core/util.rs"), ["core"]);
        assert!(owners(&g, "crates/core/notes.txt").is_empty());
    }
}
