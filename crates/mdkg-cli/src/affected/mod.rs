//! `mdkg affected`: the tests a change needs.
//!
//! It reads the paths that differ between two commits and decides whether
//! a run can be selective, and if so which packages, crates and suites it
//! needs. Any doubt fails closed to a full run: a wrong selection would
//! hide breakage. A decision, selective or full, is a success (exit 0);
//! only bad arguments or an unreadable repository fail.
//!
//! Code paths and adapter metadata come from the two commits; selection
//! rules come from one committed graph authority. Uncommitted checkout edits,
//! filters and sparse entries cannot change either input. `--head` defaults
//! to `HEAD`. In 0.7.3 the authority is the repository-top graph, even when
//! the caller selects a nested root.
//!
//! A path whose content at the head is what main had where the head last
//! met it (their merge base) came from main unchanged: main's own runs
//! tested it, so the selection skips it and lists it in `from_main`. Main
//! is `--main`, else `mdkg.mainRef`, else `main`; when none resolves,
//! nothing is skipped. Under `--targets` only an explicit `--main` naming
//! a branch counts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use mdkg_format::Code;
use mdkg_graph::GraphError;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{self, Report};

pub mod config;
pub mod git;
pub mod glob;
pub mod go;
pub mod modules;
pub mod rust;
pub mod select;
pub mod tree;

/// The selection, or the reasons it fell back to a full run.
#[derive(Debug, Serialize, JsonSchema)]
pub struct AffectedData {
    pub base: Rev,
    pub head: Rev,
    pub mode: Mode,
    /// Why the run is full; empty when it is selective. Sorted.
    pub reasons: Vec<Reason>,
    /// What a selective run needs; empty when the run is full.
    pub selected: Selected,
    /// Every changed path the selection judged and what it maps to,
    /// sorted by path. Paths in `from_main` aren't here.
    pub paths: Vec<PathOut>,
    /// The main branch `from_main` compares against; absent when no main
    /// ref resolved and none was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main: Option<MainOut>,
    /// Changed paths whose content at the head equals main's at their merge
    /// base: they came from main unchanged, so the selection skips them.
    /// Sorted by path. Always present in this build's output; optional in
    /// the schema, for older output.
    #[schemars(skip_serializing_if = "Vec::is_empty")]
    pub from_main: Vec<FromMain>,
    /// Selection notes for a selective run: paths using whole-crate
    /// fallback because no finer Rust rule covers them, and rule targets
    /// the head doesn't have, which were skipped.
    #[schemars(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<SelectionWarning>,
    /// Where the head's snapshot for the language adapters went, and the
    /// places passed over on the way; absent when no adapter needed one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotOut>,
}

/// Where the head's snapshot went (`tree::places`, in order).
#[derive(Debug, Serialize, JsonSchema)]
pub struct SnapshotOut {
    /// The place used; `null` when the snapshot wasn't written, and the run
    /// is full with an `adapter_failed` reason: `error` says why.
    pub place: Option<tree::Place>,
    /// The places tried before it (or all of them), each with why. Empty
    /// with `place` null when the head's tree was refused before any place
    /// was tried (a symlink in it, say).
    pub skipped: Vec<tree::Skipped>,
    /// Why the snapshot wasn't written; absent when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
pub struct SelectionWarning {
    pub path: String,
    pub packages: Vec<String>,
    pub detail: String,
}

/// The main ref, and why nothing was skipped when that's so.
#[derive(Debug, Serialize, JsonSchema)]
pub struct MainOut {
    /// The ref as given (`--main`) or found (`mdkg.mainRef`, `main`).
    pub rev: String,
    /// The commit it names; absent when it names none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// The head's merge bases with it, sorted; empty when none was used.
    pub merge_bases: Vec<String>,
    /// Why no path was compared (for example, the head is on main).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A changed path that came from main unchanged.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FromMain {
    pub path: String,
    /// git's status letter between the base and the head.
    pub status: String,
    /// For a rename or copy, the path at the base; it matches main too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

/// A revision as given, and the commit it names.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Rev {
    pub rev: String,
    /// The commit SHA; absent when `rev` names no commit, or when nothing
    /// was resolved (a partial clone).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// `selective`: run only `selected`. `full`: run everything.
pub enum Mode {
    // Run only `selected`.
    Selective,
    // Run everything.
    Full,
}

/// Why a run falls back to full: `base_missing`, `head_missing`,
/// `base_not_ancestor` (or git can't tell), `partial_clone`,
/// `unreadable_path`, `global_file`, `unmatched_path`, `rule_selects_all`,
/// `adapter_failed`, `package_removed`, `config_invalid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    // `--base` names no commit (for example, history a shallow clone lacks).
    BaseMissing,
    // `--head` names no commit.
    HeadMissing,
    // `--base` isn't an ancestor of `--head`, or git can't tell.
    BaseNotAncestor,
    // A partial clone: reading a missing object would fetch it.
    PartialClone,
    // A path git reported that isn't UTF-8, or a status it doesn't define.
    UnreadablePath,
    // A file every test depends on changed.
    GlobalFile,
    // No adapter maps the path and no rule matches it.
    UnmatchedPath,
    // A rule selects everything for the path.
    RuleSelectsAll,
    // An adapter couldn't build its graph.
    AdapterFailed,
    // A package the base had is gone at the head.
    PackageRemoved,
    // The `affected` section of the config is invalid.
    ConfigInvalid,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
pub struct Reason {
    pub code: ReasonCode,
    /// The changed path it concerns, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub detail: String,
}

/// What a selective run needs. Each list is sorted and has no duplicates.
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct Selected {
    /// Go packages, per module.
    pub go: Vec<GoSelection>,
    /// Rust workspace crates.
    pub rust: Vec<String>,
    /// Complete Rust execution plan. Prefer this over `rust` when present.
    #[schemars(skip_serializing_if = "Vec::is_empty")]
    pub rust_targets: Vec<RustTestSelection>,
    /// Suites named by config rules; the consumer decides what they run.
    pub suites: Vec<String>,
}

/// One selected target and the combined Cargo invocation covering it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
pub struct RustTestSelection {
    pub package: String,
    /// `package`, `lib`, `bin` or `test`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Entries share grouped invocations: deduplicate before executing the plan.
    pub cargo_args: Vec<String>,
}

impl RustTestSelection {
    fn whole(package: &str) -> Self {
        Self {
            package: package.into(),
            kind: "package".into(),
            name: None,
            cargo_args: vec!["test".into(), "-p".into(), package.into()],
        }
    }

    fn scoped(target: &config::RustTarget) -> Self {
        let mut selection = Self::whole(&target.package);
        let (kind, name) = match &target.kind {
            config::RustTargetKind::Lib => ("lib", None),
            config::RustTargetKind::Test(name) => ("test", Some(name)),
            config::RustTargetKind::Bin(name) => ("bin", Some(name)),
        };
        selection.kind = kind.into();
        selection.name = name.cloned();
        selection.cargo_args.push(format!("--{kind}"));
        if let Some(name) = name {
            selection.cargo_args.push(name.clone());
        }
        selection
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GoSelection {
    /// The module path from `go.mod`.
    pub module: String,
    /// The module's folder, relative to the repository (`.` for the top).
    pub dir: String,
    /// Import paths.
    pub packages: Vec<String>,
}

/// What accounted for a changed path: `go` or `rust` (an adapter mapped
/// it), `rule` (a config rule matched it), `global` (a global file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    Go,
    Rust,
    Rule,
    Global,
}

/// One changed path.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PathOut {
    pub path: String,
    /// git's status letter: `A`, `C`, `D`, `M`, `R` or `T`; any other
    /// letter comes with the `unreadable_path` reason.
    pub status: String,
    /// For a rename or copy, the path at the base. It is changed too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// What accounted for the path; empty when nothing did.
    pub matched_by: Vec<MatchKind>,
    /// What it selects (packages, crates, suites), sorted.
    pub selects: Vec<String>,
    /// Why it forces a full run, if it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ReasonCode>,
}

/// Runs `affected` and prints its report.
pub fn run(
    root: &Path,
    base: &str,
    head: &str,
    main: Option<&str>,
    targets: bool,
    json: bool,
) -> u8 {
    let result = decide(root, base, head, main, targets).map_err(|error| {
        if targets && error.diagnostic.hint.is_none() {
            error.with_hint("Fetch complete base/head commits locally, fix the committed config or offline metadata, and retry --targets.")
        } else { error }
    });
    let report = match result {
        Ok(data) => Report::ok(data),
        Err(e) => Report::failed(e),
    };
    output::emit("affected", json, report, print_text)
}

fn git_error(e: std::io::Error) -> GraphError {
    GraphError::failure(Code::IoGitFailed, e.to_string())
}

fn target_rules_error(detail: &str) -> GraphError {
    GraphError::failure(Code::ConfigInvalid,
        format!("cannot verify affected Rust test-target rules: {detail}"))
        .with_hint("Make the committed snapshot and offline cargo metadata available, or remove the rust_targets rules; no test selection was verified.")
}

/// The decision, without printing it.
pub fn decide(
    root: &Path,
    base: &str,
    head: &str,
    main: Option<&str>,
    strict: bool,
) -> Result<AffectedData, GraphError> {
    let repo = git::Repo::open(root).map_err(git_error)?;
    if repo.is_partial_clone().map_err(git_error)? {
        // Even resolving a revision could fetch missing objects over the
        // network in a partial clone, so decide before reading anything.
        if strict {
            return Err(GraphError::failure(Code::IoGitFailed, "cannot verify a target-aware selection in a partial clone")
                .with_hint("Use a complete local clone without --filter before requesting --targets; affected never fetches objects."));
        }
        return Ok(finish(AffectedData {
            base: Rev {
                rev: base.to_string(),
                sha: None,
            },
            head: Rev {
                rev: head.to_string(),
                sha: None,
            },
            mode: Mode::Full,
            reasons: vec![Reason {
                code: ReasonCode::PartialClone,
                path: None,
                detail: "this is a partial clone, where reading a missing object fetches it; affected reads only local objects, so clone without --filter".into(),
            }],
            selected: Selected::default(),
            paths: Vec::new(),
            main: None,
            from_main: Vec::new(),
            warnings: Vec::new(),
            snapshot: None,
        }));
    }
    let base_sha = repo.resolve_commit(base).map_err(git_error)?;
    let head_sha = repo.resolve_commit(head).map_err(git_error)?;
    let mut data = AffectedData {
        base: Rev {
            rev: base.to_string(),
            sha: base_sha.clone(),
        },
        head: Rev {
            rev: head.to_string(),
            sha: head_sha.clone(),
        },
        mode: Mode::Full,
        reasons: Vec::new(),
        selected: Selected::default(),
        paths: Vec::new(),
        main: None,
        from_main: Vec::new(),
        warnings: Vec::new(),
        snapshot: None,
    };
    let (Some(base_sha), Some(head_sha)) = (base_sha, head_sha) else {
        if strict {
            return Err(GraphError::failure(Code::IoGitFailed, "cannot verify affected base/head: a revision names no local commit")
                .with_hint("Fetch the required base and head locally or choose existing commits, then retry with --targets."));
        }
        if data.base.sha.is_none() {
            data.reasons.push(Reason {
                code: ReasonCode::BaseMissing,
                path: None,
                detail: format!("{base} names no commit here; a shallow clone may lack it"),
            });
        }
        if data.head.sha.is_none() {
            data.reasons.push(Reason {
                code: ReasonCode::HeadMissing,
                path: None,
                detail: format!("{head} names no commit"),
            });
        }
        return Ok(finish(data));
    };

    let ancestry = repo.is_ancestor(&base_sha, &head_sha);
    if !matches!(ancestry, Ok(true)) {
        if strict {
            return Err(GraphError::failure(
                Code::IoGitFailed,
                "cannot verify affected ancestry for --targets",
            )
            .with_hint(
                "Fetch complete local history and choose a base that is an ancestor of the head.",
            ));
        }
        // In a shallow clone a missing link in the history reads as "not an
        // ancestor", so say what to do about it.
        let shallow = repo.is_shallow().unwrap_or(true);
        let why = match &ancestry {
            Err(e) => format!(": {e}"),
            _ => String::new(),
        };
        data.reasons.push(Reason {
            code: ReasonCode::BaseNotAncestor,
            path: None,
            detail: if shallow {
                format!(
                    "git can't tell whether {base} is an ancestor of {head} in this shallow clone; fetch more history (or the base){why}"
                )
            } else if ancestry.is_err() {
                format!("git can't tell whether {base} is an ancestor of {head}{why}")
            } else {
                format!("{base} is not an ancestor of {head}")
            },
        });
    }

    // Code and adapter metadata come from the selected head, while the rules
    // are pinned independently to the graph authority for this operation.
    let entries = repo.tree(&head_sha).map_err(git_error)?;
    let config = match mdkg_run::orchestration::authority(&repo.top)
        .map_err(|e| format!("{}: {}", e.code, e.message))
        .and_then(|authority| config::parse(&authority.config.to_string()))
    {
        Ok(c) => c,
        Err(e) => {
            data.reasons.push(Reason {
                code: ReasonCode::ConfigInvalid,
                path: None,
                detail: e,
            });
            config::Config::defaults()
        }
    };
    if strict
        && let Some(reason) = data
            .reasons
            .iter()
            .find(|r| r.code == ReasonCode::ConfigInvalid)
    {
        return Err(target_rules_error(&reason.detail));
    }
    let tracked = tree::paths(&entries);
    let wants_tree = tracked
        .iter()
        .any(|p| rust::is_manifest_name(p) || go::is_module_path(p));
    // Written only when an adapter could use it, and removed on return.
    let written = if wants_tree {
        let mut skipped = Vec::new();
        let result = tree::write(&repo, &entries, &tree::places(), &mut skipped);
        data.snapshot = Some(SnapshotOut {
            place: result.as_ref().ok().map(|t| t.place),
            skipped,
            error: result.as_ref().err().cloned(),
        });
        match result {
            Ok(t) => Some(t),
            Err(e) => {
                data.reasons.push(Reason {
                    code: ReasonCode::AdapterFailed,
                    path: None,
                    detail: e,
                });
                None
            }
        }
    } else {
        None
    };
    let (mut go, mut rust) = match &written {
        Some(t) => (
            go::GoAdapter::discover(&t.root, &t.cache, &tracked),
            rust::RustAdapter::discover(&t.root, &t.cache, &tracked),
        ),
        None => (None, None),
    };
    let rule_targets: Vec<_> = config
        .rules
        .iter()
        .flat_map(|rule| match &rule.select {
            config::RuleSelect::Targets { rust_targets, .. } => rust_targets.clone(),
            _ => Vec::new(),
        })
        .collect();
    if strict && wants_tree && written.is_none() {
        let why = data
            .snapshot
            .as_ref()
            .and_then(|s| s.error.as_deref())
            .unwrap_or("unknown");
        return Err(target_rules_error(&format!(
            "the committed snapshot could not be created: {why}"
        )));
    }
    if !rule_targets.is_empty() || strict {
        if let Some(rust) = &mut rust {
            match rust.unsupported_rule_target(&rule_targets) {
                Ok(Some((target, reason))) => return Err(GraphError::failure(Code::ConfigInvalid,
                    format!("affected rule selects unsupported opt-in Rust target {} {:?} ({reason})", target.package, target.kind))
                    .with_hint("In 0.7.3, affected rules require Cargo default test targets. Remove this rust_targets entry or make the target a default Cargo test (test = true and no required-features); run opt-in targets explicitly outside affected.")),
                Ok(None) => {},
                Err(detail) => {
                    let detail = written.as_ref().map_or(detail.clone(), |t| t.scrub(&detail));
                    if strict { return Err(target_rules_error(&detail)); }
                    data.reasons.push(Reason { code: ReasonCode::AdapterFailed, path: None, detail });
                }
            }
        } else if !rule_targets.is_empty() {
            if strict {
                return Err(target_rules_error(
                    "the committed head has no usable Rust adapter",
                ));
            }
            data.reasons.push(Reason {
                code: ReasonCode::AdapterFailed,
                path: None,
                detail: "no Rust adapter is available for the configured target rules".into(),
            });
        }
    }
    let mut engine = select::Engine {
        config: &config,
        go: go.as_mut().map(|a| a as &mut dyn select::Adapter),
        rust: rust.as_mut().map(|a| a as &mut dyn select::Adapter),
    };
    let mut changes = repo.changes(&base_sha, &head_sha).map_err(git_error)?;
    let (main_out, from_main) =
        split_from_main(&repo, &head_sha, &entries, main, strict, &mut changes);
    data.main = main_out;
    data.from_main = from_main;
    // Submodule contents aren't in the written tree. Both endpoints matter:
    // a deleted gitlink, rename or type change may exist only at the base.
    let base_entries = repo.tree(&base_sha).map_err(git_error)?;
    let gitlinks: BTreeSet<Vec<u8>> = base_entries
        .iter()
        .chain(&entries)
        .filter(|e| e.mode == "160000")
        .map(|e| e.path.clone())
        .collect();
    let mut targets = BTreeSet::new();
    for change in changes {
        let (out, verdict) = explain(&mut engine, change, &gitlinks);
        for (code, detail) in &verdict.reasons {
            data.reasons.push(Reason {
                code: *code,
                path: Some(out.path.clone()),
                detail: detail.clone(),
            });
        }
        targets.extend(verdict.selects);
        for (path, package, why) in verdict.fallback_crates {
            let why = why
                .map(|w| format!(", and its test targets can't be derived: {w}"))
                .unwrap_or_default();
            data.warnings.push(SelectionWarning {
                path,
                packages: vec![package],
                detail: format!(
                    "no test-target rule covers this path for the package{why}; using whole-crate tests for the combined Rust selection"
                ),
            });
        }
        for (path, rule, target, all_missing) in verdict.missing_targets {
            let fallback = if all_missing {
                "; none of the rule's targets exist, so the package's whole tests run"
            } else {
                "; the rule's other targets still run"
            };
            data.warnings.push(SelectionWarning {
                path,
                packages: vec![target.package.clone()],
                detail: format!(
                    "affected.rules[{rule}] names Rust target {}, which the head doesn't have; skipped it{fallback}",
                    target_name(&target)
                ),
            });
        }
        data.paths.push(out);
    }
    data.selected = select::selection(&targets);
    if let Some(t) = &written {
        for r in &mut data.reasons {
            r.detail = t.scrub(&r.detail);
        }
    }
    if strict
        && let Some(reason) = data.reasons.iter().find(|r| {
            matches!(
                r.code,
                ReasonCode::AdapterFailed | ReasonCode::ConfigInvalid
            )
        })
    {
        return Err(target_rules_error(&reason.detail));
    }
    Ok(finish(data))
}

/// The main ref, its commit, and why it can't be used when that's so.
/// Without `--targets`: `--main` as given, else the branch
/// `mdkg.mainRef` names (the repository's own config file only), else the
/// branch `main`; `None` when nothing was given and neither names a
/// commit. Under `--targets`, the verified gate, only an explicit `--main`
/// naming a branch or remote-tracking ref counts, so no ambient setting
/// can shrink a verified selection (orchestrator, #12089).
fn main_ref(
    repo: &git::Repo,
    given: Option<&str>,
    strict: bool,
) -> Option<(String, Option<String>, Option<String>)> {
    if let Some(rev) = given {
        if !strict {
            return Some((
                rev.to_string(),
                repo.resolve_commit(rev).ok().flatten(),
                None,
            ));
        }
        let full = repo.full_ref_name(rev).ok().flatten();
        let branch = full
            .as_ref()
            .filter(|f| f.starts_with("refs/heads/") || f.starts_with("refs/remotes/"));
        return Some(match branch {
            Some(f) => (rev.to_string(), repo.resolve_commit(f).ok().flatten(), None),
            None => (
                rev.to_string(),
                repo.resolve_commit(rev).ok().flatten(),
                Some(format!(
                    "under --targets, --main must name a branch or remote-tracking ref, and {rev} names {}",
                    full.as_deref().unwrap_or("no ref")
                )),
            ),
        });
    }
    if strict {
        return None;
    }
    let mut names: Vec<String> = repo.configured_main().into_iter().collect();
    if names.first().map(String::as_str) != Some("main") {
        names.push("main".into());
    }
    names.into_iter().find_map(|name| {
        // A branch, not a tag or file that happens to share its name.
        let full = if name.starts_with("refs/") {
            name.clone()
        } else {
            format!("refs/heads/{name}")
        };
        let sha = repo.resolve_commit(&full).ok().flatten()?;
        Some((name, Some(sha), None))
    })
}

/// Moves the changes that came from main unchanged out of `changes`. A
/// change came from main when, at one of the head's merge bases with main,
/// each of its paths (both, for a rename or copy) has the same mode and
/// blob as at the head, or is absent at both. Any doubt (no main, a git
/// error, a shallow clone, the head on main itself) moves nothing.
fn split_from_main(
    repo: &git::Repo,
    head: &str,
    head_entries: &[git::TreeEntry],
    given: Option<&str>,
    strict: bool,
    changes: &mut Vec<git::Change>,
) -> (Option<MainOut>, Vec<FromMain>) {
    let Some((rev, sha, refused)) = main_ref(repo, given, strict) else {
        return (None, Vec::new());
    };
    let mut out = MainOut {
        rev,
        sha,
        merge_bases: Vec::new(),
        detail: None,
    };
    let skip = |mut out: MainOut, why: String| {
        out.detail = Some(format!("{why}; no path is skipped"));
        (Some(out), Vec::new())
    };
    if let Some(why) = refused {
        return skip(out, why);
    }
    let Some(main_sha) = out.sha.clone() else {
        let why = format!("{} names no commit", out.rev);
        return skip(out, why);
    };
    // A cut history can hide the real merge base behind an older one.
    if repo.is_shallow().unwrap_or(true) {
        return skip(out, "this is a shallow clone".into());
    }
    let bases = match repo.merge_bases(head, &main_sha) {
        Ok(b) if b.is_empty() => return skip(out, "the head shares no history with main".into()),
        Ok(b) => b,
        Err(e) => return skip(out, format!("git can't find the merge base: {e}")),
    };
    // Main's runs tested a merge base only when it isn't the head itself.
    if bases.iter().any(|b| b == head) {
        return skip(out, "the head is on main".into());
    }
    let index = |entries: &[git::TreeEntry]| -> BTreeMap<Vec<u8>, (String, String)> {
        entries
            .iter()
            .map(|e| (e.path.clone(), (e.mode.clone(), e.oid.clone())))
            .collect()
    };
    let at_head = index(head_entries);
    let mut at_bases = Vec::new();
    for b in &bases {
        match repo.tree(b) {
            Ok(entries) => at_bases.push(index(&entries)),
            Err(e) => return skip(out, format!("git can't read merge base {b}: {e}")),
        }
    }
    out.merge_bases = bases;
    let mut kept = Vec::new();
    let mut from_main = Vec::new();
    for change in changes.drain(..) {
        let known = matches!(change.status, 'A' | 'C' | 'D' | 'M' | 'R' | 'T');
        let paths: Vec<&Vec<u8>> = std::iter::once(&change.path)
            .chain(change.old_path.as_ref())
            .collect();
        let matches_main = known
            && at_bases
                .iter()
                .any(|base| paths.iter().all(|p| base.get(*p) == at_head.get(*p)));
        let path = String::from_utf8(change.path.clone());
        let old_path = change.old_path.clone().map(String::from_utf8).transpose();
        match (matches_main, path, old_path) {
            (true, Ok(path), Ok(old_path)) => from_main.push(FromMain {
                path,
                status: change.status.to_string(),
                old_path,
            }),
            _ => kept.push(change),
        }
    }
    *changes = kept;
    (Some(out), from_main)
}

/// What one changed path maps to. A rename or copy judges its old path
/// too: a rename's old path no longer exists at the head.
fn explain(
    engine: &mut select::Engine<'_>,
    change: git::Change,
    gitlinks: &BTreeSet<Vec<u8>>,
) -> (PathOut, select::PathVerdict) {
    let known = matches!(change.status, 'A' | 'C' | 'D' | 'M' | 'R' | 'T');
    let path = String::from_utf8(change.path);
    let old_path = change.old_path.map(String::from_utf8).transpose();
    let (path, old_path) = match (path, old_path) {
        (Ok(path), Ok(old_path)) if known => (path, old_path),
        (path, old_path) => {
            let path = match path {
                Ok(p) => p,
                Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
            };
            let mut verdict = select::PathVerdict::default();
            verdict.reasons.insert(
                ReasonCode::UnreadablePath,
                format!(
                    "git reported status {} or a path that isn't UTF-8",
                    change.status
                ),
            );
            let out = PathOut {
                path,
                status: change.status.to_string(),
                old_path: old_path.ok().flatten(),
                matched_by: Vec::new(),
                selects: Vec::new(),
                reason: Some(ReasonCode::UnreadablePath),
            };
            return (out, verdict);
        }
    };
    let gitlink = gitlinks.contains(path.as_bytes())
        || old_path
            .as_ref()
            .is_some_and(|p| gitlinks.contains(p.as_bytes()));
    let mut verdict = if gitlink {
        let mut v = select::PathVerdict::default();
        v.reasons.insert(
            ReasonCode::UnmatchedPath,
            "a changed submodule's contents and tests aren't in the committed snapshot".into(),
        );
        v
    } else {
        engine.judge(&path, change.status == 'D')
    };
    if !gitlink && let Some(old) = &old_path {
        let before = engine.judge(old, change.status == 'R');
        verdict.matched_by.extend(before.matched_by);
        verdict.selects.extend(before.selects);
        verdict.fallback_crates.extend(before.fallback_crates);
        verdict.missing_targets.extend(before.missing_targets);
        let what = if change.status == 'R' {
            "rename"
        } else {
            "copy"
        };
        for (code, detail) in before.reasons {
            verdict
                .reasons
                .entry(code)
                .or_insert_with(|| format!("{old} (before the {what}): {detail}"));
        }
    }
    let out = PathOut {
        path,
        status: change.status.to_string(),
        old_path,
        matched_by: verdict.matched_by.iter().cloned().collect(),
        selects: verdict
            .selects
            .iter()
            .map(|target| match target {
                select::Target::RustTest(target) => select::Target::Rust(target.package.clone()),
                target => target.clone(),
            })
            .collect::<BTreeSet<_>>()
            .iter()
            .map(select::Target::label)
            .collect(),
        reason: verdict.reasons.keys().next().copied(),
    };
    (out, verdict)
}

/// A rule target as the config spells it, for warnings.
fn target_name(target: &config::RustTarget) -> String {
    match &target.kind {
        config::RustTargetKind::Lib => format!("{} lib", target.package),
        config::RustTargetKind::Test(name) => format!("{} test {name}", target.package),
        config::RustTargetKind::Bin(name) => format!("{} bin {name}", target.package),
    }
}

/// Sorts everything and sets the mode: full whenever there is a reason.
fn finish(mut data: AffectedData) -> AffectedData {
    data.paths
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.old_path.cmp(&b.old_path)));
    data.reasons.sort();
    data.reasons.dedup();
    data.warnings.sort();
    data.warnings.dedup();
    data.from_main
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.old_path.cmp(&b.old_path)));
    if data.reasons.is_empty() {
        data.mode = Mode::Selective;
    } else {
        data.mode = Mode::Full;
        data.selected = Selected::default();
        data.warnings.clear();
    }
    data
}

fn print_text(data: &AffectedData) {
    let mode = match data.mode {
        Mode::Selective => "selective",
        Mode::Full => "full",
    };
    println!("mode: {mode}");
    // Only a snapshot that left the temporary folder is news.
    if let Some(snap) = &data.snapshot
        && !snap.skipped.is_empty()
    {
        let place = match snap.place {
            Some(tree::Place::Temp) => "the temporary folder",
            Some(tree::Place::UserCache) => "the user cache",
            Some(tree::Place::CargoTargetDir) => "$CARGO_TARGET_DIR",
            None => "nowhere",
        };
        for s in &snap.skipped {
            println!(
                "snapshot: skipped {}: {}",
                serde_json::to_value(s.place)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                output::shown_path(&s.detail)
            );
        }
        println!("snapshot: written to {place}");
    }
    for warning in &data.warnings {
        println!(
            "warning: {}: {} ({})",
            output::shown_path(&warning.path),
            output::shown_path(&warning.detail),
            warning
                .packages
                .iter()
                .map(|p| output::shown_path(p))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for r in &data.reasons {
        let detail = output::shown_path(&r.detail);
        match &r.path {
            Some(p) => println!(
                "reason: {} {}: {detail}",
                code_name(r.code),
                output::shown_path(p)
            ),
            None => println!("reason: {}: {detail}", code_name(r.code)),
        }
    }
    for m in &data.selected.go {
        for p in &m.packages {
            println!("go: {}", output::shown_path(p));
        }
    }
    for c in &data.selected.rust {
        println!("rust: {}", output::shown_path(c));
    }
    // The plan, which a target rule can make narrower than its packages:
    // `rust: mdkg` alone reads as the whole package.
    let plans: BTreeSet<&Vec<String>> = data
        .selected
        .rust_targets
        .iter()
        .map(|t| &t.cargo_args)
        .collect();
    for args in plans {
        println!("cargo: {}", output::shown_path(&args.join(" ")));
    }
    for s in &data.selected.suites {
        println!("suite: {}", output::shown_path(s));
    }
    if let Some(main) = &data.main {
        let detail = main
            .detail
            .as_ref()
            .map(|d| format!(" ({})", output::shown_path(d)))
            .unwrap_or_default();
        println!(
            "main: {}: {} path(s) came from main unchanged{detail}",
            output::shown_path(&main.rev),
            data.from_main.len()
        );
    }
}

fn code_name(code: ReasonCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}
