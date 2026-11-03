//! `mdkg doctor`: checks the setup. It covers this build,
//! the `PATH`, git, the config, the graph, the coordinator store, the
//! skill mirrors and the agents' MCP configs. Only --repair writes missing
//! clone-local graph identity records; projections are never synced. The
//! store is opened read-only and never upgraded, so doctor is safe to run
//! with any build, in any repository.
//!
//! Each check passes, warns or fails; doctor fails (exit 2) when any check
//! fails, or with `--strict` when any warns.

use std::path::{Path, PathBuf};

use clap::Args;
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::{ErrorKind, config};
use mdkg_run::program::npm_launcher;
pub(crate) use mdkg_run::program::runs_program;
use mdkg_run::projection::{Harness, Projections};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::coord::Repo;
use crate::output::{self, Report};

use mdkg_format::program::BIN;

use mdkg_coord_sqlite::git::{
    MIN_VERSION as GIT_MIN, minimum_version as git_minimum, upgrade_hint as git_upgrade_hint,
};

/// The `work.committed` values for storing work definitions in git or locally.
const WORK_COMMITTED: &[&str] = &["definitions", "none"];

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Add missing clone-local graph identity records only; never sync projections.
    #[arg(long)]
    pub repair: bool,
    /// Fail on warnings too.
    #[arg(long)]
    pub strict: bool,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Warning,
    Error,
    /// Not checked: what it needs isn't there (no graph, no git).
    Skip,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Check {
    /// The setup component, including `binary`, `path`, `git`, `config`,
    /// `graph`, `store`, `mirrors`, `mcp`, `agents`, `hooks` or `builds`;
    /// in ref mode `code_base` and the `graph.*` checks, in checkout mode
    /// `main` and a `graph.ref` warning.
    pub name: String,
    pub status: Status,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The `store` check only: the coordinator store's schema version, as
    /// its summary's `schema <n>` names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_schema: Option<u64>,
    /// The `store` check only: the store's mail schema version (`+msg<n>`),
    /// 0 for a store with no mail tables.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mail_schema: Option<u64>,
    /// The `store` check only: whether this build would upgrade the store
    /// when it next opens it (copying it first).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade_needed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorData {
    /// The folder checked: the graph found at or above where doctor ran,
    /// or that folder when there's none.
    pub root: String,
    pub checks: Vec<Check>,
    pub errors: usize,
    pub warnings: usize,
}

fn check(name: &str, status: Status, summary: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        status,
        summary: summary.into(),
        hint: None,
        store_schema: None,
        mail_schema: None,
        upgrade_needed: None,
    }
}

impl Check {
    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// The graph's folder: the nearest at or above `start` with a config, as
/// git finds its repository, or `start` itself.
pub fn find_root(start: &Path) -> PathBuf {
    start
        .ancestors()
        .find(|d| config::has_config(d))
        .map_or_else(|| start.to_path_buf(), Path::to_path_buf)
}

pub fn run(root: &Path, args: &DoctorArgs) -> u8 {
    let mut checks = vec![binary(), path_check()];
    checks.push(git_check());
    let mode = graph_ref_checks(root, &mut checks);
    // The projections' files are the checkout's in either mode.
    let mut files = root.to_path_buf();
    let projections = match &mode {
        Mode::Checkout => {
            checks.push(identity_check(root, args.repair));
            checks.push(graph_worktree_check(root));
            let (config_check, cfg) = config_check(root);
            checks.push(config_check);
            match &cfg {
                Some(cfg) => {
                    checks.push(graph_check(root, cfg));
                    checks.push(mirrors_check(root, cfg));
                }
                None => {
                    checks.push(check("graph", Status::Skip, "no readable config"));
                    checks.push(check("mirrors", Status::Skip, "no readable config"));
                }
            }
            // A config with unreadable projections fails the config check;
            // the checks below then check none.
            Projections::read(root).unwrap_or_else(|_| Projections::none())
        }
        Mode::Ref(Some(found)) => {
            files = found.top.clone();
            let (config_check, _) = judge_config(
                config::from_snapshot(found.config.clone()),
                found.config.clone(),
                Some(Ok(found.config.clone())),
                None,
            );
            checks.push(config_check);
            // `graph.ref` validated the tip strictly.
            checks.push(check(
                "graph",
                Status::Ok,
                format!("valid at {} (read from the graph ref)", short(&found.tip)),
            ));
            checks.push(check(
                "mirrors",
                Status::Skip,
                "ref mode: install mirrors the skills from the graph tip",
            ));
            Projections::from_config(&found.config).unwrap_or_else(|_| Projections::none())
        }
        Mode::Ref(None) => {
            for name in ["config", "graph", "mirrors"] {
                checks.push(check(
                    name,
                    Status::Skip,
                    "the graph ref can't be read (see graph.ref); no checkout copy stands in",
                ));
            }
            Projections::none()
        }
    };
    checks.push(store_check(&files));
    checks.push(builds_check(root));
    checks.push(progress_check(root));
    checks.push(faults_check(root));
    checks.push(mcp_check(&files, &projections));
    checks.push(agents_check(&files, &projections));
    let hook_graph = match &mode {
        Mode::Ref(Some(found)) => Some(found.as_ref()),
        _ => None,
    };
    checks.push(hooks_check(&files, &projections, hook_graph));

    let errors = checks.iter().filter(|c| c.status == Status::Error).count();
    let warnings = checks
        .iter()
        .filter(|c| c.status == Status::Warning)
        .count();
    let diagnostics = checks
        .iter()
        .filter_map(|c| {
            // The builds row already carries this summary in text output.
            // JSON retains the coded diagnostic as well as the check.
            if c.name == "builds" && !args.json {
                return None;
            }
            let d = match c.status {
                Status::Error => Diagnostic::error(
                    Code::DoctorCheckFailed,
                    format!("{}: {}", c.name, c.summary),
                ),
                Status::Warning => Diagnostic::warning(
                    Code::DoctorCheckWarned,
                    format!("{}: {}", c.name, c.summary),
                ),
                Status::Ok | Status::Skip => return None,
            };
            Some(match &c.hint {
                Some(h) => d.with_hint(h.clone()),
                None => d,
            })
        })
        .collect::<Vec<_>>();
    let strict_failed = errors == 0 && args.strict && warnings > 0;
    let mut diagnostics = diagnostics;
    if strict_failed {
        // The output contract makes `ok` false exactly when there's an error.
        diagnostics.push(Diagnostic::error(
            Code::DoctorCheckFailed,
            format!("--strict: {} warned", plural(warnings, "check")),
        ));
    }
    let failed = errors > 0 || strict_failed;
    let report = Report {
        data: Some(DoctorData {
            root: root.display().to_string(),
            checks,
            errors,
            warnings,
        }),
        diagnostics,
        failure: failed.then_some(ErrorKind::Validation),
    };
    output::emit("doctor", args.json, report, |d| {
        for c in &d.checks {
            let mark = match c.status {
                Status::Ok => "ok  ",
                Status::Warning => "warn",
                Status::Error => "ERR ",
                Status::Skip => "skip",
            };
            println!("{mark} {:<8} {}", c.name, output::shown_path(&c.summary));
            if c.name == "builds"
                && let Some(hint) = &c.hint
            {
                println!("  hint: {}", output::shown_path(hint));
            }
        }
        println!(
            "{}, {}",
            plural(d.errors, "error"),
            plural(d.warnings, "warning")
        );
    })
}

fn identity_check(root: &Path, repair: bool) -> Check {
    if mdkg_coord_sqlite::git::common_dir(root).is_err() {
        return check("graph_identity", Status::Skip, "no git common directory");
    }
    match mdkg_run::install::worktree::repair_identities(root, repair) {
        Ok(writes) if writes.is_empty() => check(
            "graph_identity",
            Status::Ok,
            "no missing graph identity records",
        ),
        Ok(writes) if repair => check(
            "graph_identity",
            Status::Ok,
            format!("wrote: {}", writes.join("; ")),
        ),
        Ok(writes) => check(
            "graph_identity",
            Status::Warning,
            format!("identity repair would write: {}", writes.join("; ")),
        )
        .hint(format!(
            "run {BIN} doctor --repair to add only these records"
        )),
        Err(e) => {
            let mut c = check("graph_identity", Status::Error, e.message);
            c.hint = e.hint;
            c
        }
    }
}

fn graph_worktree_check(root: &Path) -> Check {
    match crate::graph_repair::inspect(root) {
        Ok(Some(state)) => {
            let hint = state.hint();
            check("graph_worktree", Status::Error, state.reason).hint(hint)
        }
        Ok(None) => check("graph_worktree", Status::Ok, "no frozen graph checkout"),
        Err(e) => check("graph_worktree", Status::Error, e.diagnostic.message)
            .hint("inspect the shared graph checkout with git -C .worktrees/graph status"),
    }
}

fn progress_check(root: &Path) -> Check {
    match crate::agents::progress_status(root) {
        Ok(records) => {
            let problems: Vec<_> = records
                .iter()
                .filter_map(|r| {
                    r.problem.as_ref().map(|problem| {
                        format!(
                            "{}: {}; {}; metadata {}",
                            r.agent,
                            problem,
                            r.repair.as_deref().unwrap_or("inspect agents status"),
                            r.record.as_ref().map(Value::to_string).unwrap_or_default()
                        )
                    })
                })
                .collect();
            if problems.is_empty() {
                check(
                    "supervisor_progress",
                    Status::Ok,
                    "no interrupted or unreadable supervisor progress",
                )
            } else {
                check("supervisor_progress", Status::Warning, problems.join("; "))
            }
        }
        Err(e) => check("supervisor_progress", Status::Warning, e.diagnostic.message),
    }
}

/// Faults whose send failed, kept for the next loop boundary (A26).
fn faults_check(root: &Path) -> Check {
    let Ok(dir) = mdkg_run::state::run_dir(root) else {
        return check("faults", Status::Skip, "no git common directory");
    };
    match mdkg_run::fault::pending_all(&dir) {
        Ok(agents) if agents.is_empty() => check("faults", Status::Ok, "no pending faults"),
        Ok(agents) => {
            let each: Vec<String> = agents
                .iter()
                .map(|(agent, count)| match count {
                    Ok(n) => format!("{agent}: {}", plural(*n, "pending fault")),
                    Err(e) => format!("{agent}: pending faults unreadable ({})", e.message),
                })
                .collect();
            check("faults", Status::Warning, each.join("; ")).hint(format!(
                "a running supervisor sends them again at its next loop boundary, and `{BIN} \
                 agents up` when it starts"
            ))
        }
        Err(e) => check("faults", Status::Warning, e.message),
    }
}

fn builds_check(root: &Path) -> Check {
    let dir = match mdkg_run::state::run_dir(root) {
        Ok(dir) => dir,
        Err(_) => return check("builds", Status::Skip, "no git common directory"),
    };
    match mdkg_run::builds::live(&dir) {
        Ok(live) => {
            let warnings = live.warnings(&crate::version::build());
            if warnings.is_empty() {
                if live.processes.is_empty() {
                    return check("builds", Status::Ok, "no build inventory yet");
                }
                check(
                    "builds",
                    Status::Ok,
                    format!(
                        "{} registered launcher/MCP process(es); builds agree",
                        live.processes.len()
                    ),
                )
            } else {
                check("builds", Status::Warning, warnings.join("; "))
                    .hint("restart stale launcher and MCP processes after upgrading mdkg")
            }
        }
        Err(e) => check("builds", Status::Warning, e.message).hint(
            "inspect the local run/builds directory; process build records could not be read",
        ),
    }
}

/// Why work can't settle on the main branch yet, and the commands that
/// fix it, in order.
pub(crate) struct MainProblem {
    pub problem: String,
    pub hint: String,
    pub fix: Vec<String>,
}

/// What stops work settling on the main branch (`$MDKG_MAIN_REF`, then
/// `mdkg.mainRef`, then `main`, as the coordinator reads it): no commit
/// yet, a different branch checked out, or an override naming a branch
/// that isn't there. `None` when it resolves, or outside a git
/// repository. `new`, `claim` and `done` all need it.
pub(crate) fn main_branch_problem(dir: &Path) -> Option<MainProblem> {
    let git = |args: &[&str]| git_out(dir, args);
    git(&["rev-parse", "--git-dir"])?;
    let main = mdkg_coord_sqlite::git::main_ref(dir);
    if mdkg_coord_sqlite::git::has_ref(dir, &main) {
        return None;
    }
    let first_commit = "git add -A && git commit -m \"Start the graph\"".to_string();
    let unborn = git(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).is_none();
    let steps = |fix: Vec<String>| {
        let quoted: Vec<String> = fix.iter().map(|c| format!("`{c}`")).collect();
        format!("run {}", quoted.join(", then "))
    };
    let head = git(&["symbolic-ref", "--quiet", "--short", "HEAD"]);
    // The user's own git honors an inherited location, which mdkg's
    // hardened git ignores: clear it first, or the repair runs elsewhere.
    let inherited: Vec<&str> = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ]
    .into_iter()
    .filter(|v| std::env::var_os(v).is_some())
    .collect();
    let with_env = |mut fix: Vec<String>| {
        if !inherited.is_empty() && !fix.is_empty() {
            fix.insert(0, format!("unset {}", inherited.join(" ")));
        }
        fix
    };
    let overridden = std::env::var("MDKG_MAIN_REF").is_ok_and(|v| !v.trim().is_empty());
    if overridden && !(unborn && head.as_deref() == Some(main.as_str())) {
        return Some(MainProblem {
            problem: format!("$MDKG_MAIN_REF names `{main}`, which doesn't exist"),
            hint: "set MDKG_MAIN_REF to your main branch, or unset it to use \
                   `git config mdkg.mainRef`"
                .into(),
            fix: Vec::new(),
        });
    }
    let fix = match head {
        // Checked out, but no commit yet.
        Some(branch) if branch == main => vec![first_commit],
        // Another branch checked out (`master`): name it as main.
        Some(branch) => {
            let name = format!("git config mdkg.mainRef {}", shell_word(&branch));
            if unborn {
                vec![name, first_commit]
            } else {
                vec![name]
            }
        }
        None => {
            return Some(MainProblem {
                problem: format!("work settles on `{main}`, which doesn't exist"),
                hint: format!(
                    "create `{main}`, or name your main branch: `git config mdkg.mainRef <branch>`"
                ),
                fix: Vec::new(),
            });
        }
    };
    let problem = if unborn {
        format!("the repository has no commit yet, so there's no `{main}` for work to settle on")
    } else {
        format!("work settles on `{main}`, which doesn't exist")
    };
    let fix = with_env(fix);
    Some(MainProblem {
        problem,
        hint: steps(fix.clone()),
        fix,
    })
}

/// `s` as one word in a shell line: as it is when plain, else in single
/// quotes.
fn shell_word(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// git's output in `dir`, when it succeeds: the coordinator's own
/// hardened git, so an inherited GIT_DIR doesn't point it elsewhere.
fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = mdkg_coord_sqlite::git::command(dir)
        .args(args)
        .run(None)
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether the branch work settles on is there.
fn main_check(root: &Path) -> Check {
    match main_branch_problem(root) {
        Some(p) => check("main", Status::Warning, p.problem).hint(p.hint),
        None if git_out(root, &["rev-parse", "--git-dir"]).is_none() => {
            check("main", Status::Skip, "not in a git repository")
        }
        None => check(
            "main",
            Status::Ok,
            format!(
                "work settles on `{}`",
                mdkg_coord_sqlite::git::main_ref(root)
            ),
        ),
    }
}

/// The graph doctor reads in ref mode: the committed config at the graph
/// tip, as git objects. Doctor never binds the command to the graph ref,
/// which could replay an intent or refresh the cache: it stays a read.
struct RefGraph {
    graph: mdkg_graph::graphref::GraphRef,
    top: PathBuf,
    tip: String,
    graph_id: String,
    config: Value,
}

/// Which graph the remaining checks read.
enum Mode {
    /// 0.7.0's: the checkout's `.mdkg`, settled on main.
    Checkout,
    /// The graph ref, or `None` when it can't be read (`graph.ref` says
    /// why); no checkout copy stands in.
    Ref(Option<Box<RefGraph>>),
}

/// A value from the clone's common git config file only.
fn common_value(top: &Path, common: &Path, key: &str) -> Option<String> {
    git_out(
        top,
        &[
            "config",
            "--file",
            &common.join("config").to_string_lossy(),
            "--get",
            key,
        ],
    )
}

/// The graph-ref checks of the Config section's doctor table
/// (edd-01a18867, R37), and the code base in place of 0.7.0's `main`
/// check (row 14). In checkout mode: `graph.ref` warns and the `main`
/// check stays, since checkout mode still settles on main.
fn graph_ref_checks(root: &Path, checks: &mut Vec<Check>) -> Mode {
    if git_out(root, &["rev-parse", "--git-dir"]).is_none() {
        checks.push(main_check(root));
        return Mode::Checkout;
    }
    let graph = match mdkg_graph::graphref::resolve(root) {
        Ok(Some(graph)) => graph,
        Ok(None) => {
            checks.push(
                // Info, not a warning: checkout mode stays supported until an
                // explicit `--setup`, so `--strict` passes it (dec-01a18ee0
                // item 10).
                check(
                    "graph.ref",
                    Status::Ok,
                    "info: checkout mode: the graph is this checkout's .mdkg, settled on main",
                )
                .hint(format!(
                    "`{BIN} install --setup local` (or `personal`, `professional`) moves this \
                     clone to the graph ref"
                )),
            );
            checks.push(main_check(root));
            return Mode::Checkout;
        }
        Err(e) => {
            checks.push(
                check("graph.ref", Status::Error, e.to_string())
                    .hint("fix the mdkg.graph.* keys in the repository's own git config"),
            );
            return Mode::Ref(None);
        }
    };
    let Ok(top) = mdkg_coord_sqlite::git::toplevel(root) else {
        checks.push(check(
            "graph.ref",
            Status::Error,
            "can't find this checkout's top folder",
        ));
        return Mode::Ref(None);
    };
    if let Some(step) = common_value(&top, &graph.common, "mdkg.graph.migrating") {
        checks.push(
            check(
                "graph.migrating",
                Status::Error,
                format!("a migration to the graph ref is part-way through (step {step})"),
            )
            .hint(format!("run `{BIN} install` to finish it")),
        );
    }
    checks.push(code_base_check(&top, &graph));
    let read = graph_ref_read(&top, &graph);
    let found = match read {
        Ok(found) => {
            checks.push(check(
                "graph.ref",
                Status::Ok,
                format!(
                    "{} at {}, graph {}",
                    graph.name,
                    short(&found.tip),
                    found.graph_id
                ),
            ));
            found
        }
        Err(e) => {
            let mut c = check("graph.ref", Status::Error, e.message);
            c.hint = Some(e.hint);
            checks.push(c);
            return Mode::Ref(None);
        }
    };
    checks.extend([
        shape_check(&found),
        remote_check(&found),
        intents_check(&found),
        cache_check(&found),
        view_check(&found),
        hand_edits_check(&found),
        design_drift_check(&found),
        exclude_check(&found),
    ]);
    Mode::Ref(Some(Box::new(found)))
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

struct Refused {
    message: String,
    hint: String,
}

/// The `graph.ref` errors: missing, symbolic, equal to `mainRef`, checked
/// out on a branch, a tip that doesn't validate strictly, or an identity
/// this clone doesn't record.
fn graph_ref_read(top: &Path, graph: &mdkg_graph::graphref::GraphRef) -> Result<RefGraph, Refused> {
    let refused = |message: String, hint: &str| Refused {
        message,
        hint: hint.into(),
    };
    let install = format!("run `{BIN} install` to set up the graph ref");
    if graph.main_ref.as_deref() == Some(graph.name.as_str()) {
        return Err(refused(
            format!("{} is also mdkg.mainRef, the code base", graph.name),
            "name the code base with `git config mdkg.mainRef <ref>`, or another graph ref",
        ));
    }
    let tip = mdkg_graph::graphref::tip(top, graph).map_err(|e| refused(e.message, &install))?;
    if let Some(branch) = graph.name.strip_prefix("refs/heads/")
        && let Some(path) = checked_out(top, branch)
    {
        return Err(refused(
            format!("{} is checked out in {path}", graph.name),
            &format!("detach it: `git -C {} switch --detach`", shell_word(&path)),
        ));
    }
    // Ref mode binds the root graph at `.mdkg`; a registered folder
    // elsewhere is refused: no command reads it there (#16095).
    crate::config_cmd::root_graph_folder(top, graph, Some(&tip)).map_err(|e| Refused {
        message: e.diagnostic.message.clone(),
        hint: e.diagnostic.hint.clone().unwrap_or_default(),
    })?;
    let authority = mdkg_graph::graphref::authority_at(top, &tip)
        .map_err(|e| refused(e.message, "repair the graph's committed config"))?;
    graph
        .check_identity(&authority.graph_id)
        .map_err(|e| refused(e.message, &install))?;
    mdkg_graph::committed::load_strict(top, &tip, "", &authority.graph_id).map_err(|e| {
        refused(
            format!("the tip doesn't validate: {}", e.diagnostic.message),
            "repair the graph with a commit to the graph ref",
        )
    })?;
    Ok(RefGraph {
        graph: graph.clone(),
        top: top.to_path_buf(),
        tip,
        graph_id: authority.graph_id,
        config: authority.config,
    })
}

/// The worktree a branch is checked out in, if any.
fn checked_out(top: &Path, branch: &str) -> Option<String> {
    let list = git_out(top, &["worktree", "list", "--porcelain"])?;
    let mut path = None;
    for line in list.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p.to_string());
        } else if line.strip_prefix("branch ") == Some(&format!("refs/heads/{branch}")) {
            return path;
        }
    }
    None
}

/// `code_base`, info only: `mainRef` is what workers branch from, and no
/// command but worker worktree creation needs it (R2).
fn code_base_check(top: &Path, graph: &mdkg_graph::graphref::GraphRef) -> Check {
    let Some(main) = &graph.main_ref else {
        return check(
            "code_base",
            Status::Ok,
            "none (mdkg.mainRef is unset): nothing needs one but worker worktrees",
        )
        .hint("name it with `git config mdkg.mainRef <ref>` (`origin/main` is allowed)");
    };
    let Some(at) = git_out(
        top,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{main}^{{commit}}"),
        ],
    ) else {
        return check(
            "code_base",
            Status::Warning,
            format!("mdkg.mainRef names {main}, which doesn't resolve"),
        )
        .hint("fetch it, or name another with `git config mdkg.mainRef <ref>`");
    };
    if main.starts_with("refs/remotes/")
        && let Some(age) = fetched_days_ago(top, main)
        && age > 7
    {
        return check(
            "code_base",
            Status::Warning,
            format!("{main} at {}, last fetched {age} days ago", short(&at)),
        )
        .hint("fetch it: `git fetch`");
    }
    check("code_base", Status::Ok, format!("{main} at {}", short(&at)))
}

/// Days since the reflog's newest entry for `name`.
fn fetched_days_ago(top: &Path, name: &str) -> Option<u64> {
    let line = git_out(
        top,
        &["log", "-g", "-n1", "--date=unix", "--format=%gd", name],
    )?;
    let secs: u64 = line.rsplit_once("@{")?.1.strip_suffix('}')?.parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(secs) / 86_400)
}

fn graph_config(found: &RefGraph) -> Option<mdkg_graph::Config> {
    config::from_snapshot(found.config.clone()).ok()
}

/// `graph.shape`: graph-only, or the full shape G20 keeps for a personal
/// graph pushed to the code's own remote (G7).
fn shape_check(found: &RefGraph) -> Check {
    let Some(cfg) = graph_config(found) else {
        return check(
            "graph.shape",
            Status::Error,
            "the committed config can't be read",
        );
    };
    let paths = match mdkg_graph::graphref::graph_paths(&cfg) {
        Ok(p) => p,
        Err(e) => {
            return check("graph.shape", Status::Error, e.message)
                .hint("give each workspace a graph folder of its own, never `.`");
        }
    };
    let full = match crate::run::setup::full_shape(&found.top, &found.tip, &paths) {
        Ok(full) => full,
        Err(e) => return check("graph.shape", Status::Error, e.diagnostic.message),
    };
    if !full {
        return check("graph.shape", Status::Ok, "graph-only");
    }
    let code_remote = found
        .graph
        .main_ref
        .as_deref()
        .and_then(|m| m.strip_prefix("refs/remotes/"))
        .and_then(|r| r.split('/').next())
        .unwrap_or("origin");
    match &found.graph.remote {
        Some(r) if r.name != code_remote => check(
            "graph.shape",
            Status::Error,
            format!(
                "the graph ref holds code too (full shape), and graph.remote {} isn't the code's \
                 remote {code_remote}",
                r.name
            ),
        )
        .hint("a remote other than the code's must receive a graph-only ref"),
        _ => check(
            "graph.shape",
            Status::Ok,
            "full shape (kept for a personal graph)",
        ),
    }
}

/// `graph.remote`: local only, or a remote that exists, isn't a code ref,
/// and keeps its approved destination (G23).
fn remote_check(found: &RefGraph) -> Check {
    let Some(remote) = &found.graph.remote else {
        return check("graph.remote", Status::Ok, "none: local only");
    };
    let url = common_value(
        &found.top,
        &found.graph.common,
        &format!("remote.{}.url", remote.name),
    );
    if url.is_none() {
        return check(
            "graph.remote",
            Status::Error,
            format!(
                "mdkg.graph.remote names {}, which isn't a remote here",
                remote.name
            ),
        )
        .hint("name an existing remote: mdkg never creates one");
    }
    let short_ref = remote
        .remote_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(&remote.remote_ref);
    // The turn's and the hook's own list (R33), so the three agree on a
    // mainRef like refs/remotes/origin/release/1.0. Compared without case,
    // lowercased as the hook does (non-ASCII too): a case-folding disk
    // makes `Main` `main`.
    let protected = mdkg_run::profile::protected_refs(
        &mdkg_run::profile::ProtectedRefs {
            main_ref: found.graph.main_ref.as_deref(),
            ..Default::default()
        },
        &[],
    );
    let short_ref = short_ref.to_lowercase();
    if protected.iter().any(|p| p.to_lowercase() == short_ref) {
        return check(
            "graph.remote",
            Status::Error,
            format!(
                "mdkg.graph.remoteRef {} is a protected code branch",
                remote.remote_ref
            ),
        )
        .hint(
            "give the graph its own ref: `git config mdkg.graph.remoteRef refs/heads/mdkg-graph`",
        );
    }
    if remote.pinned.is_none() {
        // No 0.7.1 command approves a destination: nothing is pushed, and
        // nothing the user does would clear a warning, so it's reported,
        // not warned (`doctor --strict` passes after personal setup).
        return check(
            "graph.remote",
            Status::Ok,
            format!(
                "{} ({}), not approved: nothing is pushed to it (approving a graph remote \
                 arrives with graph sync in a later release)",
                remote.name, remote.remote_ref
            ),
        )
        .hint("approving a graph remote arrives with graph sync in a later release");
    }
    check(
        "graph.remote",
        Status::Ok,
        format!("{} ({}), approved", remote.name, remote.remote_ref),
    )
}

/// `graph.intents`: writes recorded but not yet published.
fn intents_check(found: &RefGraph) -> Check {
    match mdkg_graph::intent::Journal::open(&found.graph.common).and_then(|j| j.pending()) {
        Ok(p) if p.is_empty() => check("graph.intents", Status::Ok, "none pending"),
        Ok(p) => check(
            "graph.intents",
            Status::Warning,
            format!("{} pending", plural(p.len(), "intent")),
        )
        .hint(format!(
            "the next graph command replays them; `{BIN} status` runs one"
        )),
        Err(e) => check("graph.intents", Status::Warning, format!("can't read: {e}")),
    }
}

/// `graph.cache`: folders set aside from the graph cache, by kind, each
/// with its own advice (one classifier, shared with `status`).
fn cache_check(found: &RefGraph) -> Check {
    use crate::coord::graph_git::{Rescue, edit_hint, rescued};
    let folders = rescued(&found.graph.common);
    if folders.is_empty() {
        return check("graph.cache", Status::Ok, "no rescued edits");
    }
    let mut kinds: Vec<(Rescue, &str, usize)> = Vec::new();
    for (kind, label) in [
        (Rescue::HandEdit, "of hand edits"),
        (Rescue::Unclassified, "unclassified"),
        (Rescue::Unfinished, "from an unfinished write"),
    ] {
        let n = folders.iter().filter(|(k, _)| *k == kind).count();
        if n > 0 {
            kinds.push((kind, label, n));
        }
    }
    let summary: Vec<String> = kinds
        .iter()
        .map(|(_, label, n)| format!("{n} {label}"))
        .collect();
    let dir = found.graph.common.join("mdkg/graph/rescued");
    let hint: Vec<String> = kinds
        .iter()
        .map(|(kind, _, _)| edit_hint(Some(&dir), *kind))
        .collect();
    check(
        "graph.cache",
        Status::Warning,
        format!(
            "{} rescued from the graph cache ({})",
            plural(folders.len(), "folder"),
            summary.join(", ")
        ),
    )
    .hint(hint.join("\n"))
}

/// `graph.view`: the optional `.worktrees/graph`, detached at the tip.
fn view_check(found: &RefGraph) -> Check {
    let view = found.top.join(".worktrees/graph");
    if !view.join(".git").exists() {
        return check("graph.view", Status::Ok, "no view");
    }
    if let Some(branch) = git_out(&view, &["symbolic-ref", "-q", "HEAD"]) {
        return check(
            "graph.view",
            Status::Error,
            format!(
                "{} is checked out on {branch}, not detached",
                view.display()
            ),
        )
        .hint(format!(
            "detach it: `git -C {} switch --detach`",
            shell_word(&view.to_string_lossy())
        ));
    }
    if git_out(&view, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty()) {
        return check(
            "graph.view",
            Status::Warning,
            "the view holds hand edits, which are never read",
        );
    }
    if git_out(&view, &["rev-parse", "HEAD"]).as_deref() != Some(found.tip.as_str()) {
        return check("graph.view", Status::Warning, "the view is behind the tip").hint(format!(
            "`git -C {} switch --detach {}`",
            shell_word(&view.to_string_lossy()),
            found.graph.name
        ));
    }
    check("graph.view", Status::Ok, "detached at the tip")
}

/// `graph.hand_edits`: this checkout's uncommitted graph-folder changes
/// whose content isn't the graph tip's; they're never read.
fn hand_edits_check(found: &RefGraph) -> Check {
    let paths = graph_config(found)
        .and_then(|c| mdkg_graph::graphref::graph_paths(&c).ok())
        .unwrap_or_else(|| vec![".mdkg".into()]);
    let mut args = vec![
        "status",
        "--porcelain",
        "-z",
        "--no-renames",
        "--untracked-files=all",
        "--",
    ];
    let specs: Vec<String> = paths.iter().map(|p| format!(":(literal){p}")).collect();
    args.extend(specs.iter().map(String::as_str));
    // Untrimmed: the first entry's status may start with a space.
    let out = mdkg_coord_sqlite::git::command(&found.top)
        .args(&args)
        .run(None)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let Some(out) = out else {
        return check("graph.hand_edits", Status::Skip, "git status failed");
    };
    // Of the uncommitted changes, those whose content isn't the tip's: an
    // edit that matches the graph exactly isn't one.
    let differ = out
        .split('\0')
        .filter_map(|e| e.get(3..))
        .filter(|p| !p.is_empty())
        .filter(|p| worktree_blob(&found.top, p) != tip_blob(found, p))
        .count();
    if differ == 0 {
        return check("graph.hand_edits", Status::Ok, "none");
    }
    check(
        "graph.hand_edits",
        Status::Warning,
        format!(
            "{} uncommitted in this checkout's graph folders differ from the graph; they're never read",
            plural(differ, "path")
        ),
    )
    .hint(crate::coord::graph_git::hand_edit_hint(None))
}

/// The blob id `path` would have in this checkout, `None` when it's gone.
fn worktree_blob(top: &Path, path: &str) -> Option<String> {
    top.join(path)
        .is_file()
        .then(|| git_out(top, &["hash-object", "--", path]))
        .flatten()
        .map(|s| s.trim().to_owned())
}

/// The blob id of `path` at the graph tip, `None` when the tip lacks it.
fn tip_blob(found: &RefGraph, path: &str) -> Option<String> {
    git_out(
        &found.top,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{}:{path}", found.tip),
        ],
    )
    .map(|s| s.trim().to_owned())
    .filter(|s| !s.is_empty())
}

/// `graph.design_drift`, info only (G16, R43): the code base's design
/// folders against the graph tip's.
fn design_drift_check(found: &RefGraph) -> Check {
    let Some(main) = &found.graph.main_ref else {
        return check("graph.design_drift", Status::Skip, "no code base");
    };
    let Some(cfg) = graph_config(found) else {
        return check(
            "graph.design_drift",
            Status::Skip,
            "the committed config can't be read",
        );
    };
    let Ok(design) = mdkg_graph::graphref::design_paths(&cfg) else {
        return check("graph.design_drift", Status::Skip, "no design folders");
    };
    let mut args = vec![
        "diff",
        "--name-only",
        "--no-renames",
        main.as_str(),
        found.tip.as_str(),
        "--",
    ];
    let specs: Vec<String> = design.iter().map(|p| format!(":(literal){p}")).collect();
    args.extend(specs.iter().map(String::as_str));
    let Some(out) = git_out(&found.top, &args) else {
        return check(
            "graph.design_drift",
            Status::Skip,
            format!("{main} doesn't resolve"),
        );
    };
    let differ: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    if differ.is_empty() {
        return check(
            "graph.design_drift",
            Status::Ok,
            format!("{main}'s design matches the graph"),
        );
    }
    let mut shown = differ
        .iter()
        .take(5)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if differ.len() > 5 {
        shown.push_str(", …");
    }
    let c = check(
        "graph.design_drift",
        Status::Ok,
        format!(
            "info: {} differ from {main}: {shown}",
            plural(differ.len(), "design file")
        ),
    );
    if found.graph.publish_design {
        c.hint("publishing design to the code base arrives with a later release")
    } else {
        c
    }
}

/// The exclusion check: `node edit`'s `.mdkg-edit/` copies stay out of
/// every commit (R45).
fn exclude_check(found: &RefGraph) -> Check {
    let path = found.graph.common.join("info/exclude");
    // The shared reader, by bytes: a non-UTF-8 file can hold the rule.
    // Asked of git itself: a rule a later negation undoes isn't one.
    let held = crate::run::setup::excluded(&found.top, mdkg_run::install::worktree::EDIT_IGNORE)
        .unwrap_or(false);
    if held {
        return check(
            "graph.exclude",
            Status::Ok,
            format!("{} is excluded", mdkg_run::install::worktree::EDIT_IGNORE),
        );
    }
    check(
        "graph.exclude",
        Status::Warning,
        format!(
            "{} doesn't exclude {}, where `node edit` keeps its copies",
            path.display(),
            mdkg_run::install::worktree::EDIT_IGNORE
        ),
    )
    .hint(format!("run `{BIN} install`, which adds it"))
}

/// This build: version, commit and where it runs from.
fn binary() -> Check {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "an unknown path".into());
    check(
        "binary",
        Status::Ok,
        format!("{BIN} {} at {exe}", crate::version::LINE),
    )
}

/// Every `name` on `$PATH`, in order (absolute entries only, as the
/// coordinator's git search does).
pub(crate) fn on_path(name: &str) -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let exts: Vec<String> = if cfg!(windows) {
        vec![".exe".into(), ".cmd".into(), String::new()]
    } else {
        vec![String::new()]
    };
    let mut found = Vec::new();
    for dir in std::env::split_paths(&path).filter(|d| d.is_absolute()) {
        for ext in &exts {
            let p = dir.join(format!("{name}{ext}"));
            if p.is_file() && !found.contains(&p) {
                found.push(p);
            }
        }
    }
    found
}

/// Whether a program is mdkg 0.6.0, the TypeScript CLI (npm's link into
/// `node_modules/mdkg`, or a script run by node). The 0.7 npm package,
/// also `mdkg`, is a script too, but one with binaries beside it.
fn is_typescript_mdkg(p: &Path) -> bool {
    if npm_launcher(p).is_some() {
        return false;
    }
    let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = real.to_string_lossy().replace('\\', "/");
    if s.contains("node_modules/mdkg/") {
        return true;
    }
    let head = std::fs::File::open(&real).ok().and_then(|f| {
        use std::io::Read;
        let mut buf = [0u8; 128];
        let n = f.take(128).read(&mut buf).ok()?;
        Some(String::from_utf8_lossy(&buf[..n]).into_owned())
    });
    head.is_some_and(|h| {
        h.starts_with("#!") && h.lines().next().is_some_and(|l| l.contains("node"))
    })
}

/// Whether running `mdkg` (and this build's own name) runs this build.
fn path_check() -> Check {
    let exe = std::env::current_exe().ok();
    let mine = on_path(BIN.as_str());
    let mut problems = Vec::new();
    let mut hints = Vec::new();
    match (mine.first(), &exe) {
        (None, _) => {
            problems.push(format!("`{BIN}` isn't on PATH"));
            hints.push(
                "add the folder holding it to PATH (cargo installs into ~/.cargo/bin)".to_string(),
            );
        }
        (Some(first), Some(exe)) if !runs_program(first, exe) => {
            problems.push(format!(
                "`{BIN}` on PATH is {}, not this build",
                first.display()
            ));
            hints.push(format!(
                "run `{BIN} doctor` with the one on PATH, or reorder PATH"
            ));
        }
        _ => {}
    }
    // Until the rename, `mdkg` may still be 0.6.0; after it, 0.6.0 ahead
    // of this build on PATH runs instead of it.
    let mdkg = on_path("mdkg");
    if let Some(first) = mdkg.first()
        && is_typescript_mdkg(first)
    {
        if BIN == "mdkg" {
            problems.push(format!(
                "`mdkg` on PATH is mdkg 0.6.0 (TypeScript) at {}",
                first.display()
            ));
            hints.push(
                "remove it with `npm uninstall -g mdkg`, or put this build's folder first on PATH"
                    .to_string(),
            );
        } else if problems.is_empty() {
            return check(
                "path",
                Status::Ok,
                format!("`{BIN}` on PATH is this build; `mdkg` is still 0.6.0 (TypeScript)"),
            );
        }
    }
    if problems.is_empty() {
        check("path", Status::Ok, format!("`{BIN}` on PATH is this build"))
    } else {
        check("path", Status::Warning, problems.join("; ")).hint(hints.join("; "))
    }
}

/// `1 error`, `2 errors`.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// `git version 2.47.1 (Apple Git-154)` as (2, 47, 1).
fn git_version(line: &str) -> Option<(u64, u64, u64)> {
    let v = line.trim().strip_prefix("git version ")?;
    let mut parts = v.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // Source builds without release tags can report e.g. 2.45.GIT. The
    // major/minor still establish the floor; an unspecified patch is zero.
    let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

fn git_check() -> Check {
    let (program, path) = mdkg_coord_sqlite::git::git_program();
    let out = std::process::Command::new(program)
        .arg("--version")
        .env("PATH", path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let line = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            let minimum = git_minimum();
            return check(
                "git",
                Status::Error,
                format!(
                    "git at {} isn't installed, or doesn't run; mdkg requires Git {minimum} or later",
                    program.display()
                ),
            )
            .hint(git_upgrade_hint());
        }
    };
    git_version_check(program, &line)
}

fn git_version_check(program: &Path, line: &str) -> Check {
    let minimum = git_minimum();
    match git_version(line) {
        Some(v) if v >= GIT_MIN => check(
            "git",
            Status::Ok,
            format!("{line} at {}", program.display()),
        ),
        Some(_) => check(
            "git",
            Status::Error,
            format!(
                "{line} at {} is too old; mdkg requires Git {minimum} or later \
                 (`--attr-source`)",
                program.display()
            ),
        )
        .hint(git_upgrade_hint()),
        None => check(
            "git",
            Status::Error,
            format!(
                "can't read git's version from {line:?} at {}; mdkg requires Git {minimum} or later",
                program.display()
            ),
        )
        .hint(git_upgrade_hint()),
    }
}

/// Install and doctor must agree before install opens a coordinator or writes
/// projections: an older Git cannot safely set up worker worktrees.
pub(crate) fn require_git() -> Result<(), mdkg_graph::GraphError> {
    let check = git_check();
    if check.status == Status::Ok {
        Ok(())
    } else {
        Err(mdkg_graph::GraphError::new(
            ErrorKind::Validation,
            Diagnostic::error(Code::IoGitFailed, check.summary),
        )
        .with_hint(format!(
            "{}; install and install --undo both need supported Git to access the coordinator",
            check.hint.unwrap_or_else(git_upgrade_hint)
        )))
    }
}

/// Whether a string is a lowercase hyphenated RFC 9562 UUID: init mints a
/// v7, and a migrated identity-v2 graph keeps its own, of any version.
fn is_graph_id(s: &str) -> bool {
    mdkg_format::Uuid::parse(s).is_ok()
}

/// The config: present, readable by this build, and the 0.7.0 keys
/// (`graph_id`, `work.committed`) well formed.
fn config_check(root: &Path) -> (Check, Option<mdkg_graph::Config>) {
    if !config::has_config(root) {
        return (
            check(
                "config",
                Status::Error,
                format!("no {} here or above", config::CONFIG_PATH),
            )
            .hint(crate::init::missing_config_hint(root)),
            None,
        );
    }
    let raw: Value = std::fs::read_to_string(root.join(config::CONFIG_PATH))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    // The mail settings run from the committed authority, not this file.
    let repo = Repo::open(root).ok();
    let committed = repo.as_ref().map(crate::msg::committed_config);
    let unmigrated = repo.as_ref().map(crate::migrate::unmigrated_hint);
    judge_config(config::load(root), raw, committed, unmigrated)
}

/// The config check of a loaded config and its raw value: the checkout's
/// in checkout mode, the graph tip's in ref mode. `committed`, when
/// known, is the authority the mail settings are read from, and
/// `unmigrated` the hint for a graph with no `graph_id`.
fn judge_config(
    loaded: Result<mdkg_graph::Config, mdkg_graph::GraphError>,
    raw: Value,
    committed: Option<Result<Value, mdkg_graph::GraphError>>,
    unmigrated: Option<String>,
) -> (Check, Option<mdkg_graph::Config>) {
    let cfg = match loaded {
        Ok(c) => c,
        Err(e) => {
            let mut c = check("config", Status::Error, e.diagnostic.message.clone());
            if let Some(h) = &e.diagnostic.hint {
                c = c.hint(h.clone());
            }
            return (c, None);
        }
    };
    let mut problems = Vec::new();
    let mut warn = Vec::new();
    let mut hints = Vec::new();
    match raw.get("graph_id") {
        None => {
            warn.push("no graph_id: a 0.6.0-era graph".to_string());
            // What `migrate` itself says for this branch (e2e 0.7.1 B14).
            hints.push(unmigrated.unwrap_or_else(|| {
                format!("`{BIN} migrate` moves a 0.6.0 graph to 0.7 (dry run first)")
            }));
        }
        Some(Value::String(s)) if is_graph_id(s) => {}
        Some(_) => problems.push("graph_id isn't a lowercase UUID".to_string()),
    }
    match &committed {
        Some(Ok(config)) => problems.extend(
            crate::msg::mail_settings_errors(config)
                .into_iter()
                .map(|e| format!("committed: {}", e.diagnostic.message)),
        ),
        Some(Err(e)) => problems.push(format!(
            "the committed config can't be read: {}",
            e.diagnostic.message
        )),
        None => {}
    }
    let committed = raw.pointer("/work/committed");
    match committed {
        None => {}
        Some(Value::String(s)) if WORK_COMMITTED.contains(&s.as_str()) => {}
        Some(_) => problems.push("work.committed must be `definitions` or `none`".to_string()),
    }
    // The launcher's own reading, so a config doctor passes is one it runs.
    match mdkg_run::orchestration::Names::from_config(&raw) {
        Err(e) => problems.push(e.message),
        Ok(names) if mdkg_run::orchestration::leaves_project(names.worktree_pattern()) => warn.push(
            "orchestration.worktree leads more than one folder above the main checkout, where the launcher makes no worktree"
                .to_string(),
        ),
        Ok(_) => {}
    }
    match Projections::from_config(&raw) {
        Err(e) => problems.push(e.message),
        // The old targets, where they still say what the list does, are
        // what migrate folded into it.
        Ok(p)
            if p.declared()
                && raw
                    .pointer("/customization/skill_mirrors/targets")
                    .is_some()
                && {
                    let mut targets = mdkg_run::projection::legacy_targets(&raw);
                    targets.sort_unstable();
                    targets.dedup();
                    targets != p.mirrors()
                } =>
        {
            warn.push(
                "customization.skill_mirrors.targets is ignored: the projections list names the skill mirrors, and they differ"
                    .to_string(),
            );
            hints.push("remove customization.skill_mirrors.targets from the config".to_string());
        }
        Ok(_) => {}
    }
    let profile = match committed.and_then(Value::as_str) {
        Some("none") => "C",
        _ => "B",
    };
    if !problems.is_empty() {
        return (
            check("config", Status::Error, problems.join("; ")),
            Some(cfg),
        );
    }
    if !warn.is_empty() {
        let mut c = check("config", Status::Warning, warn.join("; "));
        if !hints.is_empty() {
            c = c.hint(hints.join("; "));
        }
        return (c, Some(cfg));
    }
    let graph = raw.get("graph_id").and_then(Value::as_str).unwrap_or("");
    (
        check(
            "config",
            Status::Ok,
            format!(
                "graph {graph}, profile {profile}, config schema {}",
                cfg.schema_version
            ),
        ),
        Some(cfg),
    )
}

fn graph_check(root: &Path, cfg: &mdkg_graph::Config) -> Check {
    match mdkg_graph::validate::validate(root, cfg) {
        Ok(r) if r.ok() => check(
            "graph",
            Status::Ok,
            format!("valid, {}", plural(r.warnings.len(), "warning")),
        ),
        Ok(r) => check(
            "graph",
            Status::Error,
            format!(
                "{}, {}",
                plural(r.errors.len(), "error"),
                plural(r.warnings.len(), "warning")
            ),
        )
        .hint(format!("`{BIN} validate` lists them")),
        Err(e) => check("graph", Status::Error, e.diagnostic.message.clone()),
    }
}

fn mirrors_check(root: &Path, cfg: &mdkg_graph::Config) -> Check {
    let diags = crate::skills::mirror_check(root, cfg, None);
    let skills = std::fs::read_dir(root.join(crate::skills::SKILLS_DIR))
        .map(|e| {
            e.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .count()
        })
        .unwrap_or(0);
    if diags.is_empty() && cfg.skill_mirror_targets.is_empty() {
        return check(
            "mirrors",
            Status::Ok,
            format!(
                "{} in {}; no skill mirror is enabled",
                plural(skills, "skill"),
                crate::skills::SKILLS_DIR
            ),
        );
    }
    if diags.is_empty() {
        return check(
            "mirrors",
            Status::Ok,
            format!(
                "{} mirrored to {}",
                plural(skills, "skill"),
                cfg.skill_mirror_targets.join(", ")
            ),
        );
    }
    let mut codes: Vec<&str> = diags.iter().map(|d| d.code.as_str()).collect();
    codes.dedup();
    check(
        "mirrors",
        Status::Warning,
        format!("{}: {}", plural(diags.len(), "problem"), codes.join(", ")),
    )
    .hint(format!(
        "`{BIN} skills validate` lists them, each with its fix"
    ))
}

/// `2+msg3` as (2, Some(3)).
fn parse_label(label: &str) -> Option<(u64, Option<u64>)> {
    let (schema, mail) = match label.split_once("+msg") {
        Some((s, m)) => (s, Some(m.parse().ok()?)),
        None => (label, None),
    };
    Some((schema.parse().ok()?, mail))
}

/// The coordinator store, read-only: whether this build reads it as it
/// is, would upgrade it (copying it first), or refuses it.
fn store_check(root: &Path) -> Check {
    let repo = match Repo::open(root) {
        Ok(r) => r,
        Err(_) => {
            return check(
                "store",
                Status::Skip,
                "not in a git repository: no coordinator",
            );
        }
    };
    let path = match mdkg_coord_sqlite::git::store_path(&repo.top) {
        Ok(p) => p,
        Err(e) => return check("store", Status::Error, format!("can't find the store: {e}")),
    };
    let conn = match mdkg_coord_sqlite::Store::open_raw_read_only(&path) {
        Ok(None) => {
            return check(
                "store",
                Status::Ok,
                "no store yet: the first claim, `new` or message creates it",
            );
        }
        Ok(Some(conn)) => conn,
        Err(e) => {
            return check(
                "store",
                Status::Error,
                format!("can't read {}: {e}", path.display()),
            );
        }
    };
    let read = |key: &str| -> Result<Option<String>, rusqlite::Error> {
        use rusqlite::OptionalExtension;
        let has_meta = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'")?
            .exists([])?;
        if !has_meta {
            return Ok(None);
        }
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    };
    let (schema, mail) = match (read("schema"), read("msg_schema")) {
        (Ok(s), Ok(m)) => (s, m),
        (Err(e), _) | (_, Err(e)) => {
            return check(
                "store",
                Status::Error,
                format!("can't read {}: {e}", path.display()),
            )
            .hint("stop other mdkg processes and run doctor again; `store snapshots` lists its copies");
        }
    };
    let Some(schema) = schema else {
        return check(
            "store",
            Status::Error,
            format!("{} has no schema version", path.display()),
        );
    };
    let label = match &mail {
        Some(m) => format!("{schema}+msg{m}"),
        None => schema.clone(),
    };
    let build = mdkg_coord_sqlite::build_label();
    let copies = mdkg_coord_sqlite::backups(&path)
        .map(|b| b.len())
        .unwrap_or(0);
    let at = format!(
        "{} (schema {label}, {copies} pre-upgrade copies)",
        path.display()
    );
    let (Some((have, have_mail)), Some((want, want_mail))) =
        (parse_label(&label), parse_label(&build))
    else {
        return check(
            "store",
            Status::Error,
            format!("{at}: can't read its version"),
        );
    };
    let have_mail = have_mail.unwrap_or(0);
    let want_mail = want_mail.unwrap_or(0);
    let upgrade = have <= want && have_mail <= want_mail && (have < want || have_mail < want_mail);
    let c = if have > want || have_mail > want_mail {
        check(
            "store",
            Status::Error,
            format!("{at}: newer than this build ({build})"),
        )
        .hint("a newer build upgraded this store, so this one refuses it: install that build")
    } else if upgrade {
        check("store", Status::Warning, format!("{at}: older than this build ({build})")).hint(format!(
            "the next coordinator command this build runs upgrades it, copying it first; `{BIN} store restore` puts a copy back"
        ))
    } else {
        check("store", Status::Ok, at)
    };
    // Structured, so no reader parses the summary (Omni, #12874).
    Check {
        store_schema: Some(have),
        mail_schema: Some(have_mail),
        upgrade_needed: Some(upgrade),
        ..c
    }
}

/// The `mdkg` server command each agent's MCP config starts: (harness,
/// config file, command).
fn mcp_commands(root: &Path) -> Vec<(Harness, &'static str, String)> {
    let json = |rel: &str| -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(root.join(rel)).ok()?).ok()
    };
    let mut found = Vec::new();
    if let Some(v) = json(".mcp.json")
        && let Some(c) = v
            .pointer("/mcpServers/mdkg/command")
            .and_then(Value::as_str)
    {
        found.push((Harness::Claude, ".mcp.json", c.to_string()));
    }
    if let Some(v) = json("opencode.json")
        && let Some(c) = v.pointer("/mcp/mdkg/command/0").and_then(Value::as_str)
    {
        found.push((Harness::OpenCode, "opencode.json", c.to_string()));
    }
    if let Ok(text) = std::fs::read_to_string(root.join(".codex/config.toml"))
        && let Ok(doc) = text.parse::<toml_edit::DocumentMut>()
        && let Some(c) = doc
            .get("mcp_servers")
            .and_then(|t| t.get("mdkg"))
            .and_then(|t| t.get("command"))
            .and_then(|c| c.as_str())
    {
        found.push((Harness::Codex, ".codex/config.toml", c.to_string()));
    }
    found
}

/// Whether the agents' instructions are this build's: the mdkg-workflow
/// skill as this build ships it, and, where the agents-md projection is
/// enabled, AGENTS.md's mdkg block as `install` writes it (for any command
/// the MCP configs name).
fn agents_check(root: &Path, projections: &Projections) -> Check {
    use mdkg_run::install::{BLOCK_END, BLOCK_START, agents_block};
    let mut problems = Vec::new();
    let mut hints = Vec::new();
    let mut block_ok = false;
    // Before projections were declared, a missing block was a skip; once
    // agents-md is declared, it's a sync install hasn't run.
    let missing = |what: &str| -> Check {
        if projections.declared() {
            check(
                "agents",
                Status::Warning,
                format!("agents-md is enabled, but {what}"),
            )
            .hint(format!("run `{BIN} install` to write it"))
        } else {
            check("agents", Status::Skip, what.to_string())
                .hint(format!("`{BIN} install` writes it"))
        }
    };
    let text = if projections.agents_md() {
        let Ok(text) = std::fs::read_to_string(root.join("AGENTS.md")) else {
            return missing("there's no AGENTS.md");
        };
        let Some(start) = text.find(BLOCK_START) else {
            return missing("AGENTS.md has no mdkg block");
        };
        Some((text, start))
    } else {
        None
    };
    match text
        .as_ref()
        .map(|(t, start)| (t, *start, t[*start..].find(BLOCK_END)))
    {
        None => {}
        Some((text, start, Some(i))) => {
            // As install writes it, whichever line endings the file keeps.
            let block = text[start..start + i + BLOCK_END.len()].replace("\r\n", "\n");
            let mut commands: Vec<String> =
                mcp_commands(root).into_iter().map(|(_, _, c)| c).collect();
            commands.extend([BIN.to_string(), "mdkg".to_string()]);
            if commands
                .iter()
                .any(|c| agents_block(c).trim_end() == block.trim_end())
            {
                block_ok = true;
            } else {
                problems.push("AGENTS.md's mdkg block isn't this build's".to_string());
                hints.push(format!("run `{BIN} install` to rewrite it"));
            }
        }
        Some((_, _, None)) => {
            problems.push("AGENTS.md's mdkg block has no end marker".to_string());
            hints.push(format!("run `{BIN} install` to rewrite it"));
        }
    }
    // The skill by its version: another build's warns; this build's version
    // with other content is the project's own additions.
    let skill = root
        .join(crate::skills::SKILLS_DIR)
        .join("mdkg-workflow")
        .join("SKILL.md");
    let shipped = skill_version(crate::init::WORKFLOW_SKILL);
    let mut extended = false;
    if let Ok(t) = std::fs::read_to_string(&skill) {
        let t = t.replace("\r\n", "\n");
        let found = skill_version(&t);
        if found != shipped {
            problems.push(format!(
                "the mdkg-workflow skill is version {}, not this build's {}",
                found.as_deref().unwrap_or("none"),
                shipped.as_deref().unwrap_or("none")
            ));
            hints.push(format!(
                "bring in this build's: remove .mdkg/skills/mdkg-workflow, run `{BIN} skills add mdkg-workflow`, then re-apply the project's own additions"
            ));
        } else if t != crate::init::WORKFLOW_SKILL {
            extended = true;
        }
    }
    if !problems.is_empty() {
        return check("agents", Status::Warning, problems.join("; ")).hint(hints.join("; "));
    }
    let skill = if extended {
        "the mdkg-workflow skill is this build's version, with the project's own changes"
    } else {
        "the mdkg-workflow skill is this build's"
    };
    let summary = if block_ok {
        format!("AGENTS.md's mdkg block is this build's; {skill}")
    } else {
        format!("{skill}; the agents-md projection isn't enabled")
    };
    check("agents", Status::Ok, summary)
}

/// Whether each enabled hook projection's file holds its canonical hooks.
/// OpenCode runs a project's plugins without asking, so its file is
/// checked byte for byte.
fn hooks_check(root: &Path, projections: &Projections, graph: Option<&RefGraph>) -> Check {
    let enabled: Vec<Harness> = Harness::ALL
        .into_iter()
        .filter(|h| projections.hooks(*h))
        .collect();
    if enabled.is_empty() && !projections.git_hooks() {
        return check("hooks", Status::Skip, "no hook projection is enabled").hint(format!(
            "`{BIN} install --enable hooks:claude,hooks:codex,hooks:opencode` writes them"
        ));
    }
    let mut problems: Vec<String> = enabled
        .iter()
        .filter_map(|h| {
            if *h == Harness::OpenCode
                && let Some(found) = graph
            {
                // Preserve the exact bytes, including the final newline:
                // git_out trims output and cannot judge an exact default.
                let path = h.adapter().hook_layout().canonical;
                let object = format!("{}:{path}", found.tip);
                let small_blob = git_out(&found.top, &["cat-file", "-t", &object]).as_deref()
                    == Some("blob")
                    && git_out(&found.top, &["cat-file", "-s", &object])
                        .and_then(|size| size.parse::<u64>().ok())
                        .is_some_and(|size| size <= mdkg_run::install::hook_files::HOOK_FILE_CAP);
                // The tip is immutable: check type and size before
                // collecting the blob's output in memory.
                let canonical = small_blob
                    .then(|| {
                        mdkg_coord_sqlite::git::command(&found.top)
                            .args(["show", &object])
                            .run(None)
                            .ok()
                            .filter(|out| out.status.success())
                            .and_then(|out| String::from_utf8(out.stdout).ok())
                    })
                    .flatten();
                mdkg_run::install::hook_files::status_with_canonical(root, *h, canonical.as_deref())
            } else {
                mdkg_run::install::hook_files::status(root, *h)
            }
        })
        .collect();
    let git = projections.git_hooks();
    if git && let Some(p) = mdkg_run::install::hook_files::git_status(root) {
        problems.push(p);
    }
    // The capability matrix: a rule whose events a harness lacks is named,
    // never silently skipped.
    let matrix: Vec<String> = enabled
        .iter()
        .map(|h| h.name())
        .chain(git.then_some("git"))
        .filter_map(crate::hook::unsupported_line)
        .chain(crate::hook::not_built_line())
        .collect();
    let matrix = if matrix.is_empty() {
        String::new()
    } else {
        format!("; unsupported: {}", matrix.join("; "))
    };
    if problems.is_empty() {
        let names: Vec<String> = enabled
            .iter()
            .map(|h| format!("hooks:{}", h.name()))
            .chain(git.then(|| "hooks:git".to_string()))
            .collect();
        check(
            "hooks",
            Status::Ok,
            format!("{} as their canonical files say{matrix}", names.join(", ")),
        )
    } else {
        check(
            "hooks",
            Status::Warning,
            format!("{}{matrix}", problems.join("; ")),
        )
        .hint(format!("run `{BIN} install` to sync them"))
    }
}

/// A skill's `version`, read by the same frontmatter parser `skills`
/// uses.
fn skill_version(text: &str) -> Option<String> {
    let parsed = mdkg_format::frontmatter::parse(text, "SKILL.md").ok()?;
    let version = parsed.frontmatter.get("version")?.as_str()?;
    Some(version.to_string())
}

/// Whether each enabled MCP projection's config starts a program that's
/// there.
fn mcp_check(root: &Path, projections: &Projections) -> Check {
    let enabled: Vec<Harness> = Harness::ALL
        .into_iter()
        .filter(|h| projections.mcp(*h))
        .collect();
    if enabled.is_empty() {
        return check("mcp", Status::Skip, "no MCP projection is enabled").hint(format!(
            "`{BIN} install --enable mcp:claude,mcp:codex,mcp:opencode` writes them"
        ));
    }
    let commands: Vec<_> = mcp_commands(root)
        .into_iter()
        .filter(|(h, _, _)| enabled.contains(h))
        .collect();
    // Declared projections whose config names no mdkg server: a sync
    // install hasn't run. Before they were declared, only what's there.
    let unsynced: Vec<String> = if projections.declared() {
        enabled
            .iter()
            .filter(|h| !commands.iter().any(|(c, _, _)| c == *h))
            .map(|h| {
                format!(
                    "mcp:{} is enabled, but its config names no mdkg server",
                    h.name()
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    if commands.is_empty() && unsynced.is_empty() {
        return check(
            "mcp",
            Status::Skip,
            "no agent MCP config names an mdkg server",
        )
        .hint(format!(
            "`{BIN} install` writes them for Claude Code, Codex and OpenCode"
        ));
    }
    let mut missing: Vec<String> = commands
        .iter()
        .filter(|(_, _, c)| {
            let p = Path::new(c);
            if p.components().count() > 1 {
                !p.is_file()
            } else {
                on_path(c).is_empty()
            }
        })
        .map(|(_, file, c)| format!("{file} runs `{c}`, which isn't on PATH"))
        .collect();
    missing.extend(unsynced);
    if missing.is_empty() {
        let files: Vec<&str> = commands.iter().map(|(_, f, _)| *f).collect();
        check(
            "mcp",
            Status::Ok,
            format!("mdkg server found for {}", files.join(", ")),
        )
    } else {
        check("mcp", Status::Warning, missing.join("; ")).hint(format!(
            "run `{BIN} install` again to point them at `{BIN}`"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_versions_parse() {
        assert_eq!(
            git_version("git version 2.47.1 (Apple Git-154)"),
            Some((2, 47, 1))
        );
        assert_eq!(
            git_version("git version 2.40.0.windows.1"),
            Some((2, 40, 0))
        );
        assert_eq!(git_version("hello"), None);
        assert_eq!(git_version("git version 2.45.GIT"), Some((2, 45, 0)));
    }

    #[test]
    fn git_floor_requires_attribute_isolation() {
        let program = Path::new("/fixture/bin/git");
        for line in [
            "git version 2.39.5 (Apple Git-154)",
            "git version 2.40.9",
            "unknown",
        ] {
            let check = git_version_check(program, line);
            assert_eq!(check.status, Status::Error, "{line}");
            assert!(check.summary.contains(line), "{}", check.summary);
            assert!(check.summary.contains("2.41.0"), "{}", check.summary);
            assert!(check.hint.unwrap().contains("brew install git"));
        }
        for line in [
            "git version 2.41.0",
            "git version 2.41.0.windows.1",
            "git version 2.41.GIT",
            "git version 3.0.0",
        ] {
            assert_eq!(
                git_version_check(program, line).status,
                Status::Ok,
                "{line}"
            );
        }
    }

    #[test]
    fn graph_id_shape() {
        assert!(is_graph_id("0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b"));
        assert!(is_graph_id("6f1c2b3a-4d5e-4f60-8a9b-9c0d1e2f3a4b"));
        assert!(!is_graph_id("0192F3A4-7b2c-7def-8a9b-1c2d3e4f5a6b"));
        assert!(!is_graph_id("0192f3a4-7b2c-7def-ca9b-1c2d3e4f5a6b"));
        assert!(!is_graph_id("0192f3a47b2c7def8a9b1c2d3e4f5a6b"));
        assert!(!is_graph_id("0192f3a4"));
    }

    #[test]
    fn npm_launchers_run_their_binaries() {
        let dir = std::env::temp_dir().join(format!("mdkg-doctor-npm-{}", uuid::Uuid::now_v7()));
        let pkg = dir.join("lib/node_modules/mdkg");
        let real = pkg.join("node_modules/.bin_real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        let launcher = pkg.join("run-mdkg.js");
        // cargo-dist 0.32.0's launcher, as its npm package ships it.
        std::fs::write(
            &launcher,
            "#!/usr/bin/env node\n\nconst { run } = require(\"./binary\");\nrun(\"mdkg\");\n",
        )
        .unwrap();
        std::fs::write(
            pkg.join("install.js"),
            "#!/usr/bin/env node\n\nconst { install } = require(\"./binary\");\ninstall(false);\n",
        )
        .unwrap();
        std::fs::write(pkg.join("run-other.js"), "#!/usr/bin/env node\n").unwrap();
        std::fs::write(real.join("mdkg"), "").unwrap();
        std::fs::write(dir.join("other/mdkg"), "").unwrap();
        let link = |name: &str, target: &Path| {
            let l = dir.join("bin").join(name);
            std::os::unix::fs::symlink(target, &l).unwrap();
            l
        };
        let bin = link("mdkg", &launcher);
        let exe = real.join("mdkg");

        assert!(runs_program(&bin, &exe));
        assert!(runs_program(&exe, &exe));
        assert!(!runs_program(&bin, &dir.join("other/mdkg")));
        assert!(!is_typescript_mdkg(&bin));
        // The package's other scripts start something else.
        assert!(!runs_program(
            &link("install", &pkg.join("install.js")),
            &exe
        ));
        assert!(!runs_program(
            &link("noop", &pkg.join("run-other.js")),
            &exe
        ));
        // The right name with the wrong body: a no-op, or one line missing.
        let genuine = std::fs::read_to_string(&launcher).unwrap();
        for body in [
            "#!/usr/bin/env node\n",
            "#!/usr/bin/env node\n\nrun(\"mdkg\");\n",
            "#!/usr/bin/env node\n\nconst { run } = require(\"./binary\");\n",
        ] {
            std::fs::write(&launcher, body).unwrap();
            assert!(!runs_program(&bin, &exe), "{body:?}");
        }
        std::fs::write(&launcher, genuine).unwrap();
        assert!(runs_program(&bin, &exe));
        // A launcher for another of the package's binaries.
        std::fs::write(real.join("mdkg-other"), "").unwrap();
        assert!(!runs_program(&bin, &real.join("mdkg-other")));

        // 0.6.0's script, with no binaries beside it.
        std::fs::remove_dir_all(pkg.join("node_modules")).unwrap();
        assert!(!runs_program(&bin, &exe));
        assert!(is_typescript_mdkg(&bin));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn store_labels_parse() {
        assert_eq!(parse_label("2+msg3"), Some((2, Some(3))));
        assert_eq!(parse_label("1"), Some((1, None)));
        assert_eq!(parse_label("x"), None);
    }
}
