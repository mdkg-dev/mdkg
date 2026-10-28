//! `mdkg affected`: the tests a change needs.
//!
//! It reads the paths that differ between two commits and decides whether
//! a run can be selective, and if so which packages, crates and suites it
//! needs. Any doubt fails closed to a full run: a wrong selection would
//! hide breakage. A decision, selective or full, is a success (exit 0);
//! only bad arguments or an unreadable repository fail.
//!
//! It reads the two commits, never the checkout: uncommitted edits, a
//! different branch checked out, filters or sparse entries can't change
//! the answer. `--head` defaults to `HEAD`.

use std::collections::BTreeSet;
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
    /// Every changed path and what it maps to, sorted by path.
    pub paths: Vec<PathOut>,
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
    /// Suites named by config rules; the consumer decides what they run.
    pub suites: Vec<String>,
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
pub fn run(root: &Path, base: &str, head: &str, json: bool) -> u8 {
    let report = match decide(root, base, head) {
        Ok(data) => Report::ok(data),
        Err(e) => Report::failed(e),
    };
    output::emit("affected", json, report, print_text)
}

fn git_error(e: std::io::Error) -> GraphError {
    GraphError::failure(Code::IoGitFailed, e.to_string())
}

/// The decision, without printing it.
pub fn decide(root: &Path, base: &str, head: &str) -> Result<AffectedData, GraphError> {
    let repo = git::Repo::open(root).map_err(git_error)?;
    if repo.is_partial_clone().map_err(git_error)? {
        // Even resolving a revision could fetch missing objects over the
        // network in a partial clone, so decide before reading anything.
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
    };
    let (Some(base_sha), Some(head_sha)) = (base_sha, head_sha) else {
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

    // Everything below reads the head commit, never the checkout.
    let entries = repo.tree(&head_sha).map_err(git_error)?;
    let config = match tree::read(&repo, &entries, mdkg_graph::config::CONFIG_PATH)
        .and_then(|bytes| bytes.ok_or_else(|| "the head has no .mdkg/config.json".to_string()))
        .and_then(|bytes| {
            String::from_utf8(bytes).map_err(|_| ".mdkg/config.json isn't UTF-8".to_string())
        })
        .and_then(|text| {
            // Use the same format/version guard as graph commands, but only
            // on the committed config. No checkout or coordinator is read.
            let entry = entries
                .iter()
                .find(|e| e.path == mdkg_graph::config::CONFIG_PATH.as_bytes())
                .ok_or("the head has no .mdkg/config.json")?;
            let snapshot = tree::write(&repo, std::slice::from_ref(entry))?;
            mdkg_graph::config::check_version(&snapshot.root)
                .map_err(|e| snapshot.scrub(&e.to_string()))?;
            config::parse(&text)
        }) {
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
    let tracked = tree::paths(&entries);
    let wants_tree = tracked
        .iter()
        .any(|p| rust::is_manifest_name(p) || go::is_module_path(p));
    // Written only when an adapter could use it, and removed on return.
    let written = if wants_tree {
        match tree::write(&repo, &entries) {
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
    let mut engine = select::Engine {
        config: &config,
        go: go.as_mut().map(|a| a as &mut dyn select::Adapter),
        rust: rust.as_mut().map(|a| a as &mut dyn select::Adapter),
    };
    let changes = repo.changes(&base_sha, &head_sha).map_err(git_error)?;
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
        data.paths.push(out);
    }
    data.selected = select::selection(&targets);
    if let Some(t) = &written {
        for r in &mut data.reasons {
            r.detail = t.scrub(&r.detail);
        }
    }
    Ok(finish(data))
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
        selects: verdict.selects.iter().map(select::Target::label).collect(),
        reason: verdict.reasons.keys().next().copied(),
    };
    (out, verdict)
}

/// Sorts everything and sets the mode: full whenever there is a reason.
fn finish(mut data: AffectedData) -> AffectedData {
    data.paths
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.old_path.cmp(&b.old_path)));
    data.reasons.sort();
    data.reasons.dedup();
    if data.reasons.is_empty() {
        data.mode = Mode::Selective;
    } else {
        data.mode = Mode::Full;
        data.selected = Selected::default();
    }
    data
}

fn print_text(data: &AffectedData) {
    let mode = match data.mode {
        Mode::Selective => "selective",
        Mode::Full => "full",
    };
    println!("mode: {mode}");
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
    for s in &data.selected.suites {
        println!("suite: {}", output::shown_path(s));
    }
}

fn code_name(code: ReasonCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}
