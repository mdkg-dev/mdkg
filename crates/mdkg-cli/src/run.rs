//! Agent execution and `mdkg install`: the CLI
//! over the `mdkg-run` crate. The launcher reaches mail and leases through
//! this same binary's JSON contract, as any other harness would.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::ValueEnum;
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::{ErrorKind, GraphError};
use mdkg_run::install::{DEFAULT_SERVER, InstallReport};
use mdkg_run::orchestration::Names;
use mdkg_run::state::{self, SpecFlags};
use mdkg_run::turn::{self, TurnOptions, TurnReport};
use mdkg_run::{CliMail, Harness, Outcome, Role, RunError};

use mdkg_coord_sqlite::git;

use crate::coord::Repo;
use crate::output::{self, Report};
use mdkg_format::program::BIN;

// Receipt delivery will call this adapter; keep its immutable-input boundary
// independently testable while that launcher seam is integrated.
#[allow(dead_code)]
pub(crate) mod receipt;
pub(crate) mod setup;

/// Clap's local wrapper over the registry identity (no independent value set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessArg(pub Harness);

macro_rules! cli_harnesses {
    ($(($variant:ident, $id:literal, $adapter:ident)),* $(,)?) => {
        #[allow(non_upper_case_globals)]
        impl HarnessArg {
            $(pub const $variant: Self = Self(Harness::$variant);)*
            const ALL: &'static [Self] = &[$(Self::$variant),*];
        }
    };
}
mdkg_run::harness_registry!(cli_harnesses);

impl ValueEnum for HarnessArg {
    fn value_variants<'a>() -> &'a [Self] {
        Self::ALL
    }
    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.0.as_str()))
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RoleArg {
    Worker,
    Reviewer,
}

/// Resolved execution settings, internal to `agents up`.
#[derive(Debug, Clone)]
pub(crate) struct AgentArgs {
    pub agent: String,
    pub harness: Option<HarnessArg>,
    pub role: Option<RoleArg>,
    pub worktree: bool,
    pub cwd: Option<PathBuf>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub sandbox: Option<String>,
    pub approvals: Option<String>,
    pub reports_to: Option<String>,
    pub allow: Vec<String>,
    pub protect: Vec<String>,
    pub base: Option<String>,
    pub fresh: bool,
    pub binary: Option<PathBuf>,
    pub prompt: Option<String>,
    pub reply_kind: Option<String>,
    pub limit: i64,
    pub timeout: Option<u64>,
    pub keep_logs: bool,
    pub dry_run: bool,
    pub allow_zen: bool,
    pub definition: bool,
}

pub(crate) fn graph_error(e: RunError) -> GraphError {
    let kind = match e.kind {
        mdkg_run::ErrorKind::Usage => ErrorKind::Usage,
        mdkg_run::ErrorKind::NotFound => ErrorKind::NotFound,
        mdkg_run::ErrorKind::Failure => ErrorKind::Failure,
        mdkg_run::ErrorKind::Conflict => ErrorKind::Conflict,
    };
    // Codes from the mail commands pass through; any other unknown one is
    // reported as a mail failure, so the code set stays closed.
    let code = Code::parse(&e.code).unwrap_or(Code::RunMailFailed);
    let mut d = Diagnostic::error(code, e.message);
    d.hint = e.hint;
    d.path = e.path;
    GraphError::new(kind, d)
}

fn flags(a: &AgentArgs) -> SpecFlags {
    SpecFlags {
        definition: None,
        harness: a.harness.map(|h| h.0),
        role: a.role.map(|r| match r {
            RoleArg::Worker => Role::Worker,
            RoleArg::Reviewer => Role::Reviewer,
        }),
        cwd: a
            .cwd
            .as_ref()
            // Stored canonical: the folder checked is the folder saved.
            .map(|c| {
                std::fs::canonicalize(c)
                    .or_else(|_| std::path::absolute(c))
                    .unwrap_or_else(|_| c.clone())
            }),
        model: a.model.clone(),
        effort: a.effort.clone(),
        mode: a.mode.clone(),
        sandbox: a.sandbox.clone(),
        approvals: a.approvals.clone(),
        reports_to: a.reports_to.clone(),
        allow: a.allow.clone(),
        protect: a.protect.clone(),
        base: a.base.clone(),
        fresh: a.fresh,
    }
}

fn options_with_config(
    a: &AgentArgs,
    mdkg: PathBuf,
    repo: &Repo,
    config: &serde_json::Value,
) -> Result<TurnOptions, GraphError> {
    refuse_main_root(repo)?;
    mdkg_run::orchestration::agent::check_execution(config, &a.agent).map_err(graph_error)?;
    let policy = mdkg_run::policy::Policy::for_agent(config, &a.agent).map_err(graph_error)?;
    Ok(TurnOptions {
        names: Names::from_config(config).map_err(graph_error)?,
        // A person's run bills as the agent's policy says too: its
        // subscription, and the key only where the config allows it.
        auth: policy.auth,
        // Its turns' environment: the allowlist unless its policy says
        // otherwise (mdkg_run::env).
        env: policy.env,
        // The operator's standing instructions come on every turn, a
        // manual passes as well as automatic watches.
        instructions: crate::agents::instructions(
            &a.agent,
            config.get("agents").and_then(|g| g.get(&a.agent)),
        )?,
        main_ref: turn_main_ref(&repo.top)?,
        roots: worktrees(&repo.top)?,
        task: a.prompt.clone(),
        reply_kind: a.reply_kind.clone(),
        dry_run: a.dry_run,
        timeout: a.timeout.map(Duration::from_secs),
        keep_logs: a.keep_logs,
        limit: usize::try_from(a.limit).unwrap_or(usize::MAX),
        mdkg,
        flags: flags(a),
        allow_zen: a.allow_zen,
        common_dir: Some(
            git::common_dir(&repo.top)
                .and_then(std::fs::canonicalize)
                .map_err(|e| {
                    graph_error(RunError::failure(
                        "run.cwd_outside",
                        format!("can't find this repository's git folder: {e}"),
                    ))
                })?,
        ),
        launcher_root: Some(repo.top.clone()),
        graph_root: Some(repo.root.clone()),
        // Where git places the graph root (the checked top): the pin every
        // mdkg the launcher starts for an agent must match.
        trusted_worktree: Some(repo.top.clone()),
        ..TurnOptions::default()
    })
}

/// The turn's main ref. Ref mode: the code base as configured, a full ref
/// that may be `refs/remotes/origin/main`, from the config value only,
/// never `$MDKG_MAIN_REF`, since the turn's protected list reads it too
/// (R4, C4b); without one, none: mdkg never requires main (row 16,
/// orchestrator #14254). Checkout mode reads it as 0.7.0 does. Config
/// that can't be read refuses the turn.
fn turn_main_ref(top: &Path) -> Result<Option<String>, GraphError> {
    match mdkg_graph::graphref::resolve(top) {
        Ok(Some(g)) => Ok(g.main_ref),
        Ok(None) => Ok(Some(crate::coord::main_ref(top))),
        Err(e) => Err(crate::coord::graph_git::graph_error(e.code, e.message)),
    }
}

/// git for the launcher's own checks of its checkout (`symbolic-ref`, a
/// `config --file` read, which run no hooks): found on the absolute entries
/// of $PATH only, with no GIT_* variable, and every hook event switched off
/// besides (audit, 2026-10-22).
fn launcher_git() -> std::process::Command {
    let (program, path) = git::git_program();
    let mut c = std::process::Command::new(program);
    c.env("PATH", path).env("GIT_OPTIONAL_LOCKS", "0");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") && k != "GIT_OPTIONAL_LOCKS" {
            c.env_remove(k);
        }
    }
    c.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
    ]);
    git::switch_off_hooks(&mut c, &[]);
    c
}

/// The launcher never runs from a checkout of main: main changes only
/// through a reviewed merge, and under the launcher's pins mdkg commits
/// nothing onto it. Its root is a worktree on its own branch
/// (`.worktrees/graph` on `mdkg-graph`), which reaches main through a pull
/// request (orchestrator, 2026-10-22). Read as a full ref, so a tag named
/// main can't hide it, and in any case where the disk ignores case.
///
/// Checkout mode only. In ref mode the launcher writes no checkout: graph
/// writes go to the graph ref, so it runs from any checkout, main's
/// included (edd-01a18867, row 15; R18).
fn refuse_main_root(repo: &Repo) -> Result<(), GraphError> {
    // The launcher keeps its checkout as its root, so ref mode is read from
    // the clone's config. One that can't be read keeps the refusal.
    if repo.mode.is_some() || matches!(mdkg_graph::graphref::resolve(&repo.top), Ok(Some(_))) {
        return Ok(());
    }
    let main = crate::coord::main_ref(&repo.top);
    let main = main.strip_prefix("refs/heads/").unwrap_or(&main);
    let out = launcher_git()
        .arg("-C")
        .arg(&repo.top)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .map_err(|e| {
            graph_error(RunError::failure(
                "run.branch_refused",
                format!("can't read the branch of {}: {e}", repo.top.display()),
            ))
        })?;
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let want = format!("refs/heads/{main}");
    // Letter case counts as ready_to_write counts it: core.ignorecase, or a
    // platform whose disks fold by default.
    let folds = git::common_dir(&repo.top)
        .ok()
        .and_then(|c| {
            launcher_git()
                .arg("config")
                .arg("--file")
                .arg(c.join("config"))
                .args(["--get", "core.ignorecase"])
                .output()
                .ok()
        })
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
        || cfg!(any(target_vendor = "apple", target_os = "windows"));
    let same = if folds {
        head.eq_ignore_ascii_case(&want)
    } else {
        head == want
    };
    if out.status.success() && same {
        return Err(graph_error(
            RunError::usage(
                "run.branch_refused",
                format!(
                    "the launcher runs from {}, which is checked out on {main}: main changes \
                     only through a reviewed merge",
                    repo.top.display()
                ),
            )
            .with_hint(format!(
                "run it from the graph worktree: `git -C {top} worktree add .worktrees/graph -b \
                 mdkg-graph {main}` once, then `{BIN} --root {top}/.worktrees/graph{sub} agents up \
                 …`",
                top = repo.top.display(),
                sub = match repo.prefix.trim_end_matches('/') {
                    "" => String::new(),
                    p => format!("/{p}"),
                }
            )),
        ));
    }
    Ok(())
}

/// The launcher's mail and lease calls, with both pins its turns use.
fn cli_mail(root: &Path, launcher: String, opts: &TurnOptions) -> CliMail {
    let mut mail = CliMail::new(this_binary(), root.to_path_buf(), launcher);
    mail.trusted_common = opts.common_dir.clone();
    mail.trusted_worktree = opts.trusted_worktree.clone();
    mail.receipts = Some(receipt::callbacks());
    mail
}

/// This binary, as MCP configs and agent commands should name it: its bare
/// file name when that name on `PATH` is this same file or cargo-dist's npm
/// launcher for it, so upgrades that move the binary keep working. Otherwise
/// use its path as invoked, absolute
/// but with links left as they are, so `mdkg` never means whatever else
/// `PATH` finds under that name (0.6.0's CLI on many machines).
pub(crate) fn this_binary() -> PathBuf {
    let Ok(exe) = std::env::current_exe() else {
        return PathBuf::from("mdkg");
    };
    let exe = std::path::absolute(&exe).unwrap_or(exe);
    match exe.file_name() {
        Some(name)
            if on_path(name).is_some_and(|found| {
                same_file(&found, &exe) || crate::doctor::runs_program(&found, &exe)
            }) =>
        {
            PathBuf::from(name)
        }
        _ => exe,
    }
}

/// The file a shell would run for `name`: the first `PATH` entry holding
/// an executable file of that name. `None` when an empty or relative entry
/// comes first, since that one depends on the folder a command runs from,
/// which differs from agent to agent (review of 16e4ca3).
fn on_path(name: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = std::env::var_os("PATH")?;
    for d in std::env::split_paths(&path) {
        if d.as_os_str().is_empty() || !d.is_absolute() {
            return None;
        }
        let p = d.join(name);
        if std::fs::metadata(&p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) {
            return Some(p);
        }
    }
    None
}

/// Whether two paths lead to one file.
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// The program every MCP config and agent command names: `--binary` as
/// given, else this binary. Refused unless it's UTF-8: every config
/// states it as text, which would replace a stray byte and name another
/// program (review of 5582000).
pub(crate) fn server_binary(flag: Option<&Path>) -> Result<PathBuf, GraphError> {
    let program = flag.map_or_else(this_binary, Path::to_path_buf);
    if program.to_str().is_none() {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            format!(
                "{} isn't UTF-8, so no config can name it",
                program.display()
            ),
        )
        .with_hint("pass --binary <a UTF-8 path or name>"));
    }
    Ok(program)
}

/// A NO_REPLY turn's outcome: the agent answered NO_REPLY and the
/// launcher sent no final reply (e2e B17). It claims no more: the agent may
/// have mailed a reply itself (`msg send`, `channel_post`), a receipt may
/// still go to `reports_to`, and mail in a role lost during the turn stays
/// unacked (reviews of c7e7d609 and 7697bd1a).
fn no_reply(shown: usize) -> String {
    format!(
        "answered {} to {shown} message(s); the launcher sent no final reply",
        mdkg_run::digest::NO_REPLY
    )
}

pub(crate) fn print_turn(r: &TurnReport) {
    if let Some(receipt) = &r.receipt {
        if let Some(error) = &receipt.error {
            eprintln!("warning: {}", mdkg_run::digest::terminal_safe(error));
        }
        if let Some(path) = &receipt.path {
            println!(
                "turn receipt: {}",
                output::shown(&path.display().to_string())
            );
        }
    }
    for d in &r.dropped_allow {
        eprintln!(
            "warning: allow rule {} isn't passed to the agent: {}",
            output::shown(&d.rule),
            mdkg_run::digest::terminal_safe(&d.fix)
        );
    }
    // Everything here can carry text from mail or an agent (a reply that
    // echoes a body, a dry run's prompt): no escape sequence reaches the
    // operator's terminal (audit, 2026-10-22).
    let safe = mdkg_run::digest::terminal_safe;
    let what = match r.outcome {
        Outcome::Idle => "idle: no unacked mail".to_string(),
        Outcome::Replied => format!(
            "replied {} to {} message(s)",
            r.reply_id.as_deref().unwrap_or("?"),
            r.shown.len()
        ),
        Outcome::NoReply => no_reply(r.shown.len()),
        Outcome::Ran => "ran; nothing to reply to".into(),
        Outcome::DryRun => "dry run".into(),
    };
    println!(
        "{}",
        safe(&format!(
            "{} ({:?}, {:?}): {what}",
            r.agent, r.harness, r.role
        ))
    );
    for receipt in &r.codex_hook_trust {
        let phase = if receipt.preview {
            "preview"
        } else {
            "spawn attempt"
        };
        println!(
            "Codex hook trust, {phase} {}: {}",
            receipt.attempt,
            safe(receipt.reason)
        );
    }
    if let Some(why) = &r.commit_refused {
        let code = r
            .commit_refused_code
            .as_deref()
            .unwrap_or("run.commit_refused");
        println!("nothing was committed ({}): {}", safe(code), safe(why));
    }
    if let Some(id) = &r.commit_note {
        println!("told the sender so in {}", safe(id));
    }
    if !r.launcher_notes.is_empty() {
        println!(
            "acked {} launcher note(s) unseen: {}",
            r.launcher_notes.len(),
            safe(&r.launcher_notes.join(", "))
        );
    }
    if !r.not_answered.is_empty() {
        println!(
            "acked {} message(s) no reply could reach as not answered: {}",
            r.not_answered.len(),
            safe(&r.not_answered.join(", "))
        );
    }
    if let Some(why) = &r.commit_note_failed {
        println!("couldn't tell the sender so: {}", safe(why));
    }
    if let Some(c) = &r.commit {
        println!("committed {}", safe(c));
    }
    if let Some(u) = &r.usage {
        let mut parts = Vec::new();
        if let Some(c) = u.cost_usd {
            parts.push(format!("${c:.4}"));
        }
        for (n, what) in [
            (u.input_tokens, "input"),
            (u.output_tokens, "output"),
            (u.cache_read_tokens, "cache-read"),
            (u.total_tokens, "total"),
        ] {
            if let Some(n) = n {
                parts.push(format!("{n} {what} tokens"));
            }
        }
        println!("usage: {}", parts.join(", "));
    }
    if let Some(reply) = &r.reply {
        println!("{}", safe(reply));
    }
    if let Some(cmd) = &r.command {
        println!(
            "command: {}",
            safe(&format!("{} {}", cmd.program.display(), cmd.args.join(" ")))
        );
    }
    if let Some(p) = &r.prompt {
        println!("prompt:\n{}", safe(p));
    }
}

/// `--limit`'s and `--worktree`'s own argument rules, before anything else.
fn check_worktree_args(a: &AgentArgs) -> Result<(), GraphError> {
    // The same code as every other limit (D2), not clap's own refusal.
    if a.limit < 1 {
        return Err(
            GraphError::usage(Code::UsageBadLimit, "--limit must be a positive integer")
                .with_hint("pass 1 or more"),
        );
    }
    if a.worktree && a.cwd.is_some() {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "--worktree picks the agent's folder: it doesn't take --cwd",
        ));
    }
    Ok(())
}

/// Gives the agent its `--worktree` folder. Called last, once the agent and
/// the run's options were accepted, so a refused run leaves no worktree or
/// branch behind. A dry run plans it, with every check, and makes nothing:
/// the plan shows where the first turn would run.
fn give_worktree(
    a: &AgentArgs,
    repo: &Repo,
    agent: &mut mdkg_run::AgentState,
    opts: &mut TurnOptions,
) -> Result<(), GraphError> {
    if !a.worktree {
        return Ok(());
    }
    let common = git::common_dir(&repo.top).map_err(|_| {
        GraphError::usage(
            Code::RunCwdOutside,
            "can't find this repository's git folder",
        )
    })?;
    // The code base workers start from: in ref mode the configured
    // mainRef (or `$MDKG_MAIN_REF`), possibly none; in checkout mode the
    // branch work settles on, as before.
    let main = match mdkg_graph::graphref::resolve(&repo.top) {
        Ok(Some(graph)) => graph.main_ref_base,
        Ok(None) => Some(crate::coord::main_ref(&repo.top)),
        Err(e) => {
            return Err(GraphError::usage(
                Code::parse(e.code).unwrap_or(Code::RunCwdOutside),
                format!("can't read the graph-ref settings: {}", e.message),
            ));
        }
    };
    let inputs = turn::BaseInputs {
        definition: agent.spec.base.as_deref(),
        dependencies: &[],
        main_ref: main.as_deref(),
    };
    // Init leaves .gitignore alone; worktree creation owns this opt-in.
    let (planned, _) = turn::plan_agent_worktree_from(
        &common,
        &a.agent,
        &inputs,
        &opts.names,
        &agent.spec.protect,
    )
    .map_err(graph_error)?;
    if !planned.exists() {
        mdkg_run::install::worktree::ignore_worktrees(&repo.top, a.dry_run).map_err(graph_error)?;
    }
    // Only a real creation records a base: a dry run's plan (and
    // `agents up`'s preview) may differ from what the creation finds.
    let (path, made) = if a.dry_run {
        Ok((planned, None))
    } else {
        turn::agent_worktree_from(&common, &a.agent, &inputs, &opts.names)
    }
    .map_err(graph_error)?;
    let base = match made {
        // A branch made now: its base, always recorded.
        Some(mut base) => {
            base.turns = agent.turns;
            Some(base)
        }
        // An existing branch: what its reflog says it was made from, so a
        // record lost before the state was saved, or a branch made again,
        // is put right. Where the resolver still names that commit, its
        // source and notes are the record's (a lost note comes back).
        None if !a.dry_run => {
            match turn::branch_base(&common, &a.agent, &opts.names).map_err(graph_error)? {
                Some(recovered)
                    if agent.work_base.as_ref().map(|b| &b.commit) != Some(&recovered.commit) =>
                {
                    let mut base = turn::resolve_base(&common, &inputs)
                        .ok()
                        .filter(|now| now.commit == recovered.commit)
                        .unwrap_or(recovered);
                    base.turns = agent.turns;
                    Some(base)
                }
                _ => None,
            }
        }
        None => None,
    };
    if let Some(base) = base {
        agent.work_base = Some(base.clone());
        opts.work_base = Some(base);
    }
    // One the agent already has is checked as it is.
    opts.planned_worktree = a.dry_run && std::fs::symlink_metadata(&path).is_err();
    agent.spec.cwd = path.clone();
    agent.spec.pinned = None;
    // Listed before it was made: outside the main checkout (a pattern with
    // `..`), it's among the worktrees only now.
    if !opts.roots.contains(&path) {
        opts.roots.push(path.clone());
    }
    // Like --cwd: the turn applies its flags to the spec saved under the
    // lease, so an existing agent moves too.
    opts.flags.cwd = Some(path);
    Ok(())
}

/// A prepared agent's mail client and turn options. New state is saved only
/// under admission; existing state is replaced under the actual turn lease.
pub(crate) struct Watched {
    pub mail: CliMail,
    pub opts: TurnOptions,
}

/// Read-only preparation, captured once before any selected agent is written.
pub(crate) struct WatchPlan {
    repo: Repo,
    launcher: String,
    dir: PathBuf,
    existing: Option<state::AgentState>,
    pub agent: state::AgentState,
    opts: TurnOptions,
}

pub(crate) fn plan_watch(
    root: &Path,
    actor: Option<String>,
    a: &AgentArgs,
    config: &serde_json::Value,
) -> Result<WatchPlan, GraphError> {
    check_worktree_args(a)?;
    let repo = Repo::open(root)?;
    let launcher = repo.actor(actor);
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let existing = state::load(&dir, &a.agent).map_err(graph_error)?;
    let mut agent = if a.definition {
        let initial = state::resolve(&a.agent, None, flags(a), &repo.top).map_err(graph_error)?;
        match existing.clone() {
            Some(existing) => {
                state::resolve_definition(existing, initial.spec, a.fresh).map_err(graph_error)?
            }
            None => initial,
        }
    } else {
        state::resolve(&a.agent, existing.clone(), flags(a), &repo.top).map_err(graph_error)?
    };
    let mut opts = options_with_config(a, server_binary(a.binary.as_deref())?, &repo, config)?;
    let preview = AgentArgs {
        dry_run: true,
        ..a.clone()
    };
    give_worktree(&preview, &repo, &mut agent, &mut opts)?;
    if !opts.planned_worktree {
        check_cwd(&repo, &agent)?;
    }
    Ok(WatchPlan {
        repo,
        launcher,
        dir,
        existing,
        agent,
        opts,
    })
}

pub(crate) fn apply_watch(
    mut plan: WatchPlan,
    a: &AgentArgs,
    create: bool,
) -> Result<Watched, GraphError> {
    let new = plan.existing.is_none();
    if state::load(&plan.dir, &a.agent).map_err(graph_error)? != plan.existing
        || (new && !create && !a.dry_run)
    {
        return Err(graph_error(RunError::usage(
            "run.unknown_agent",
            "agent state changed during preparation; restart agents up",
        )));
    }
    give_worktree(a, &plan.repo, &mut plan.agent, &mut plan.opts)?;
    if !plan.opts.planned_worktree {
        check_cwd(&plan.repo, &plan.agent)?;
    }
    if new && !a.dry_run {
        state::save(&plan.dir, &plan.agent).map_err(graph_error)?;
    }
    let mail = cli_mail(&plan.repo.root, plan.launcher, &plan.opts);
    let opts = TurnOptions {
        flags: if a.definition || (new && a.dry_run) {
            SpecFlags {
                definition: Some(plan.agent.spec),
                ..plan.opts.flags.clone()
            }
        } else {
            SpecFlags {
                fresh: a.fresh,
                ..SpecFlags::default()
            }
        },
        ..plan.opts
    };
    Ok(Watched { mail, opts })
}

/// Refuses a git variable inherited from the caller (a git hook exports
/// GIT_DIR and GIT_INDEX_FILE): it would point every git call the launcher
/// makes, and its folder checks, at another repository.
pub(crate) fn no_inherited_git_env() -> Result<(), GraphError> {
    match turn::GIT_ENV.iter().find(|v| std::env::var_os(v).is_some()) {
        None => Ok(()),
        Some(var) => Err(GraphError::usage(
            Code::RunUnsupported,
            format!("the launcher doesn't run with {var} set; it would redirect its git"),
        )
        .with_hint(format!("run it as `env -u {var} mdkg …`"))),
    }
}

/// `mdkg install`'s flags.
#[derive(Debug, clap::Args)]
pub struct InstallArgs {
    /// Show what would change and write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Put back everything install changed here, exactly as it was;
    /// refuses where a file was edited since.
    #[arg(long)]
    pub undo: bool,
    /// The mdkg binary the MCP configs run (default: this one, by its
    /// bare name when `PATH` finds this same file, else by its path as
    /// invoked).
    #[arg(long)]
    pub binary: Option<PathBuf>,
    /// Projections to add to the config's list, comma-separated:
    /// agents-md, mcp:claude, mcp:codex, mcp:opencode,
    /// skills:.agents/skills, skills:.claude/skills, hooks:claude,
    /// hooks:codex, hooks:opencode.
    #[arg(long, value_name = "LIST", conflicts_with = "undo")]
    pub enable: Option<String>,
    /// Projections to take off the list, comma-separated; mdkg's part
    /// of their files is removed, the rest kept.
    #[arg(long, value_name = "LIST", conflicts_with = "undo")]
    pub disable: Option<String>,
    /// Move this clone to the graph ref, or set its setup again:
    /// `personal` (the graph syncs with origin), `professional` (with
    /// --graph-remote, a private repository of your own, so the company
    /// remote is never touched) or `local` (no remote). Nothing is pushed
    /// until a person approves the remote.
    #[arg(long, value_enum, conflicts_with_all = ["undo", "checkout_mode"])]
    pub setup: Option<setup::Setup>,
    /// The professional setup's graph remote: an existing remote that
    /// shares no URL with another.
    #[arg(long, value_name = "REMOTE", requires = "setup")]
    pub graph_remote: Option<String>,
    /// Allow publishing design to the code base for a professional or
    /// local setup (personal allows it already).
    #[arg(long, requires = "setup")]
    pub publish_design: bool,
    /// Create or refresh .worktrees/graph, a detached, read-only view of
    /// the graph ref.
    #[arg(long, conflicts_with_all = ["undo", "checkout_mode"])]
    pub view: bool,
    /// Install the hook projections the config lists (hooks:git,
    /// hooks:claude, hooks:codex, hooks:opencode). Hooks are opt-in:
    /// without this, install skips any not already installed in this
    /// checkout, and `--enable hooks:<name>` asks for that one. The shared
    /// hook files install writes are ignored: in .git/info/exclude for the
    /// professional setup, in .gitignore otherwise. Claude's per-user
    /// .claude/settings.local.json goes in .git/info/exclude when install
    /// creates it.
    #[arg(long, conflicts_with_all = ["undo", "checkout_mode"])]
    pub hooks: bool,
    /// Go back to checkout mode (0.7.0's layout) in this build: unsets
    /// mdkg.graph.ref and keeps the graph ref.
    #[arg(long, conflicts_with_all = ["undo", "enable", "disable"])]
    pub checkout_mode: bool,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// Whether this install moves the clone to or from the graph ref, or the
/// clone is in ref mode: then the checkout's config doesn't gate it.
pub(crate) fn installs_ref_mode(root: &Path, a: &InstallArgs) -> bool {
    let ref_mode = matches!(mdkg_graph::graphref::resolve(root), Ok(Some(_)));
    // An undo in a ref-mode clone needs no checkout `.mdkg` either (#13910).
    if a.undo {
        return ref_mode;
    }
    a.setup.is_some() || a.checkout_mode || ref_mode
}

fn install_report(
    root: &Path,
    actor: Option<String>,
    choice: &InstallArgs,
) -> Result<InstallReport, GraphError> {
    let InstallArgs {
        dry_run,
        undo,
        ref binary,
        ..
    } = *choice;
    let binary = binary.as_deref();
    let list = |s: Option<&str>| s.map_or(Ok(Vec::new()), mdkg_run::projection::names);
    let enable = list(choice.enable.as_deref()).map_err(graph_error)?;
    let disable = list(choice.disable.as_deref()).map_err(graph_error)?;
    no_inherited_git_env()?;
    crate::doctor::require_git()?;
    if undo && installs_ref_mode(root, choice) {
        return ref_undo(root, actor, dry_run);
    }
    if installs_ref_mode(root, choice) {
        return ref_install(root, actor, choice);
    }
    let repo = Repo::open(root)?;
    // v0.1 installs only where the graph is the worktree's top: elsewhere
    // the configs, the skills and `mdkg mcp`'s own graph search
    // wouldn't line up (orchestrator, 2026-10-21).
    if !repo.prefix.is_empty() {
        return Err(GraphError::usage(
            Code::InstallUnsupportedLayout,
            format!(
                "the graph is in {} below the worktree's top; install needs it at the top",
                repo.prefix.trim_end_matches('/')
            ),
        ));
    }
    // Held from before install reads anything until it's done, released
    // however it ends.
    let _turns = if dry_run {
        None
    } else {
        Some(hold_turns(&repo, actor)?)
    };
    // What install changed is recorded in the launcher's folder, which no
    // agent can write, so `--undo` can put it back.
    let dir = install_undo_dir(&repo)?;
    if undo {
        let _graph_write = if dry_run {
            None
        } else {
            crate::graph_repair::lock_writer(&repo.root)?
        };
        return mdkg_run::install::undo(&repo.top, &dir, dry_run).map_err(graph_error);
    }
    // The agents' MCP configs name this binary as `this_binary` does, or
    // `--binary` as given.
    let program = server_binary(binary)?;
    let command = program.to_str().unwrap_or_default();
    let server = mdkg_run::install::Server {
        command,
        ..DEFAULT_SERVER
    };
    let aside = if dry_run {
        None
    } else {
        mdkg_run::install::set_aside_stale(&repo.top, &dir).map_err(graph_error)?
    };
    // A dry run sets nothing aside: records a real run would set aside
    // aren't checked as this checkout's (#14381 minors).
    if dry_run && mdkg_run::install::records_stale(&repo.top, &dir) {
        // Nothing to check: they're an earlier checkout's.
    } else {
        mdkg_run::install::check_records(&repo.top, &dir).map_err(graph_error)?;
    }
    // Repairs only for an install that holds the turns and writes (#14282).
    if !dry_run {
        mdkg_run::install::repair_records(&repo.top, &dir).map_err(graph_error)?;
    }
    let graph = mdkg_run::install::worktree::GraphPlan::inspect(
        &repo.top,
        &crate::coord::main_ref(&repo.top),
    )
    .map_err(graph_error)?;
    // Validate the entire projection plan before graph setup can create a
    // checkout or ignore rule. Dry-run planning publishes no projection files.
    let mut report = mdkg_run::install::install_changing_from(
        &repo.top,
        &repo.top,
        &server,
        &enable,
        &disable,
        Some(choice.hooks),
        true,
        Some(&dir),
    )
    .map_err(graph_error)?;
    // Check ignore refusals before graph creation can write a branch,
    // worktree registration or ignore line. Dry runs must refuse too.
    let listed = mdkg_run::projection::Projections::read(&repo.top).map_err(graph_error)?;
    let (planned, _) = mdkg_run::install::opted_in(
        &repo.top,
        &repo.top,
        Some(&dir),
        &listed.with(&enable, &disable),
        choice.hooks,
        &enable,
    );
    // The refreshed list can restore a hook omitted by the old config.
    // Probe every hook (Git included), retaining existing opt-ins unless
    // this command explicitly disables them.
    let hooks: Vec<_> = mdkg_run::projection::NAMES
        .iter()
        .copied()
        .filter(|name| name.starts_with("hooks:"))
        .collect();
    let (kept_hooks, _) = mdkg_run::install::opted_in(
        &repo.top,
        &repo.top,
        Some(&dir),
        &mdkg_run::projection::Projections::of(&hooks).with(&[], &disable),
        choice.hooks,
        &enable,
    );
    let hook_targets: Vec<_> = mdkg_run::install::hook_targets(&kept_hooks)
        .into_iter()
        .chain(mdkg_run::install::personal_hook_targets(&kept_hooks))
        .collect();
    if !kept_hooks.names().is_empty()
        && graph
            .would_refresh_ignores(&hook_targets)
            .map_err(graph_error)?
    {
        return Err(GraphError::failure(
            Code::InstallUnmergeable,
            format!(
                "{} would fast-forward before install can check hooks against the updated ignore files",
                graph.path.display()
            ),
        )
        .with_hint(format!(
            "merge the configured main branch into {} to refresh it first, then run \
             install again; omitting --hooks does not disable previously installed hooks",
            graph.path.display()
        )));
    }
    ignore_hook_files_first(&repo.top, &planned, false, &dir, &|_| true, true)?;
    // The graph recheck must observe the checkout before this install edits
    // its tracked config. A setup failure must precede projection publication.
    let graph_note = graph.create(dry_run).map_err(graph_error)?;
    // GraphPlan owns its locks around setup/refresh; only acquire the writer
    // guard after those return, then retain it through projection publication.
    let _graph_write = if dry_run {
        None
    } else {
        crate::graph_repair::lock_writer(&repo.root)?
    };
    // A graph refresh may have replaced the config checked by main.rs.
    mdkg_graph::config::check_version(&repo.root)?;
    if !dry_run {
        // The hook files to come are ignored before they're written
        // (orchestrator, #14098).
        let listed = mdkg_run::projection::Projections::read(&repo.top).map_err(graph_error)?;
        let (planned, _) = mdkg_run::install::opted_in(
            &repo.top,
            &repo.top,
            Some(&dir),
            &listed.with(&enable, &disable),
            choice.hooks,
            &enable,
        );
        let first = ignore_hook_files_first(&repo.top, &planned, false, &dir, &|_| true, false)?;
        report = mdkg_run::install::install_changing_from(
            &repo.top,
            &repo.top,
            &server,
            &enable,
            &disable,
            Some(choice.hooks),
            false,
            Some(&dir),
        )
        .map_err(graph_error)?;
        let listed = mdkg_run::projection::Projections::read(&repo.top).map_err(graph_error)?;
        let (synced, _) = mdkg_run::install::opted_in(
            &repo.top,
            &repo.top,
            Some(&dir),
            &listed.with(&enable, &disable),
            choice.hooks,
            &enable,
        );
        let made = created(&report);
        report.notes.extend(ignore_hook_files(
            &repo.top, &synced, false, &dir, &made, &first,
        )?);
    }
    report.notes.extend(aside);
    report.notes.push(graph_note);
    // Only a move that keeps the graph is offered: setup's own check,
    // for the local branch and each remote's (#14187).
    let keeps = mdkg_coord_sqlite::git::toplevel(&repo.top).is_ok_and(|top| {
        std::iter::once(None)
            .chain(setup::remotes(&top).into_iter().map(Some))
            .all(|remote| {
                setup::move_keeps_graph(&top, setup::GRAPH, remote.as_deref(), setup::GRAPH).is_ok()
            })
    });
    if keeps {
        report.notes.push(format!(
            "this clone is in checkout mode; `{BIN} install --setup local` (or `personal`, \
             `professional`) moves it to the graph ref"
        ));
    }
    Ok(report)
}

/// `install --undo` in a ref-mode clone (#13910): what install wrote in
/// this checkout goes back, from its record; the graph ref isn't touched,
/// and the checkout needs no `.mdkg`. Turns are held, as for install.
fn ref_undo(
    root: &Path,
    actor: Option<String>,
    dry_run: bool,
) -> Result<InstallReport, GraphError> {
    let top = crate::coord::graph_git::git::toplevel(root).map_err(crate::coord::git_error)?;
    let _turns = if dry_run {
        None
    } else {
        held_for_install(&top, actor)?
    };
    let mut report =
        mdkg_run::install::undo(&top, &undo_dir_of(&top)?, dry_run).map_err(graph_error)?;
    // The list is the graph's, shared with every checkout: left as it is,
    // and named (orchestrator, #14149).
    // Read bounded (the binding's 1 MiB cap), each name as written, so an
    // unknown one is still named (#14349).
    let object = mdkg_graph::graphref::resolve(&top)
        .ok()
        .flatten()
        .map(|g| format!("{}:{}", g.name, mdkg_graph::config::CONFIG_PATH));
    let small = object.as_ref().is_some_and(|o| {
        git_stdout(&top, &["cat-file", "-s", o])
            .and_then(|s| s.trim().parse::<u64>().ok())
            .is_some_and(|n| n <= 1 << 20)
    });
    let listed: Option<Vec<String>> = object
        .filter(|_| small)
        .and_then(|o| git_stdout(&top, &["show", &o]))
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .map(|v| {
            v.get("projections")
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|n| n.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        });
    match listed {
        Some(names) if names.is_empty() => {}
        Some(names) => report.notes.push(format!(
            "the graph's projection list is left as it is ({}): it's shared on the graph ref; \
             `{BIN} install --disable <name>` takes one out",
            names.join(", ")
        )),
        None => report.notes.push(
            "the graph's projection list is left as it is: it's shared on the graph ref (it \
             couldn't be read here to name its entries)"
                .into(),
        ),
    }
    Ok(report)
}

/// Install in ref mode, or a move to or from it (edd-01a18867, Config,
/// "Install, the three setups"). Turns are held throughout, as in
/// checkout mode.
fn ref_install(
    root: &Path,
    actor: Option<String>,
    choice: &InstallArgs,
) -> Result<InstallReport, GraphError> {
    projection_changes(choice)?;
    let top = crate::coord::graph_git::git::toplevel(root).map_err(crate::coord::git_error)?;
    // Before setup writes anything (#13686), after an earlier checkout's
    // record at this path is set aside (#13845).
    let undo = undo_dir_of(&top)?;
    // The rollback to checkout mode writes no projection and is the way
    // back, so an unusable record never blocks it: it's named and left in
    // place (orchestrator, #13910).
    let mut unusable = None;
    let aside = if choice.checkout_mode {
        if let Err(e) = mdkg_run::install::check_records(&top, &undo) {
            // An earlier checkout's record at this path isn't broken: the
            // next projection install sets it aside (#14153).
            unusable = Some(if mdkg_run::install::records_stale(&top, &undo) {
                format!(
                    "install's record in {} is an earlier checkout's at this path; it's left \
                     in place, and the next install here sets it aside and starts a new one",
                    undo.display()
                )
            } else {
                format!(
                    "warning: install's record in {} is unusable ({}); it's left in place, and \
                     install and undo refuse it until it's fixed or removed",
                    undo.display(),
                    e.message
                )
            });
        }
        None
    } else if choice.dry_run {
        None
    } else {
        mdkg_run::install::set_aside_stale(&top, &undo).map_err(graph_error)?
    };
    // A dry run's records a real run would set aside aren't this
    // checkout's to check (#14381 minors).
    let stale_preview = choice.dry_run && mdkg_run::install::records_stale(&top, &undo);
    if !choice.checkout_mode && !stale_preview {
        mdkg_run::install::check_records(&top, &undo).map_err(graph_error)?;
    }
    let mut unbound = None;
    let _turns = if choice.dry_run {
        None
    } else {
        match held_for_install(&top, actor.clone()) {
            // No command binds a clone whose root graph is registered in
            // a folder other than `.mdkg` (#16095), so no turn runs
            // through it: the rollback, its way out, goes ahead without
            // them (Terra #16224).
            Err(_) if choice.checkout_mode && crate::config_cmd::root_folder_unsupported(&top) => {
                unbound = Some(
                    "no turns were held: no command binds this clone's graph in its \
                     registered folder"
                        .to_owned(),
                );
                None
            }
            held => held?,
        }
    };
    let waits = if choice.checkout_mode {
        Vec::new()
    } else {
        refuse_open_changesets(&top, choice.dry_run)?
    };
    // The binding's shared graph lock would block the exclusive one setup
    // takes; the turns stay held. Bound again afterwards, for what reads
    // the graph next and for releasing the turns.
    crate::coord::graph_git::unbind();
    if choice.checkout_mode {
        let notes = setup::checkout_mode(&top, choice.dry_run);
        crate::coord::graph_git::bind(&top).ok();
        let mut notes = notes?;
        notes.extend(unusable);
        notes.extend(unbound);
        return Ok(InstallReport {
            changes: Vec::new(),
            notes,
        });
    }
    let done = setup::setup(
        &top,
        &setup::Choice {
            setup: choice.setup,
            graph_remote: choice.graph_remote.as_deref(),
            publish_design: choice.publish_design,
            view: choice.view,
            dry_run: choice.dry_run,
        },
        |source, non_regular, listed, setup| {
            preview_setup_projections(source, non_regular, &top, listed, setup, choice, &undo)
        },
    );
    crate::coord::graph_git::bind(&top).ok();
    let done = done?;
    // Setup previews deliberately return graph=false because they did
    // not create or adopt a ref. An already configured, readable graph
    // can still preview its projection changes without a publication.
    let preview_graph = choice.dry_run
        && mdkg_graph::graphref::resolve(&top)
            .map_err(|e| crate::coord::graph_git::graph_error(e.code, e.message))?
            .is_some_and(|graph| {
                git_stdout(
                    &top,
                    &[
                        "rev-parse",
                        "--verify",
                        &format!("{}^{{commit}}", graph.name),
                    ],
                )
                .is_some()
            });
    let mut report = if done.graph || preview_graph {
        projections(&top, actor, choice, done.setup)?
    } else {
        InstallReport {
            changes: Vec::new(),
            notes: Vec::new(),
        }
    };
    let mut notes: Vec<String> = aside.into_iter().collect();
    notes.extend(waits);
    notes.extend(done.notes);
    notes.append(&mut report.notes);
    report.notes = notes;
    Ok(report)
}

/// Parse and reject conflicting flags before setup can change state.
fn projection_changes(
    choice: &InstallArgs,
) -> Result<(Vec<&'static str>, Vec<&'static str>), GraphError> {
    let list = |s: Option<&str>| s.map_or(Ok(Vec::new()), mdkg_run::projection::names);
    let enable = list(choice.enable.as_deref()).map_err(graph_error)?;
    let disable = list(choice.disable.as_deref()).map_err(graph_error)?;
    if let Some(both) = enable.iter().find(|n| disable.contains(n)) {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            format!("{both} is both enabled and disabled"),
        ));
    }
    Ok((enable, disable))
}

/// Preflight the selected committed source before setup publishes it.
fn preview_setup_projections(
    source: &Path,
    non_regular: &[PathBuf],
    top: &Path,
    listed: &mdkg_run::projection::Projections,
    setup: setup::Setup,
    choice: &InstallArgs,
    undo: &Path,
) -> Result<(), GraphError> {
    let (enable, disable) = projection_changes(choice)?;
    let next = listed.with(&enable, &disable);
    let (mut requested, _) =
        mdkg_run::install::opted_in(source, top, Some(undo), &next, choice.hooks, &enable);
    let professional = setup == setup::Setup::Professional;
    let mut removing = disable;
    if professional {
        for h in Harness::ALL {
            let layout = h.adapter().hook_layout();
            let name =
                mdkg_run::projection::name(&format!("hooks:{}", h.name())).map_err(graph_error)?;
            if layout.personal
                && std::fs::symlink_metadata(top.join(layout.target))
                    .is_ok_and(|m| m.file_type().is_symlink())
            {
                requested = requested.with(&[], &[name]);
                removing.retain(|n| *n != name);
            }
        }
        if !enable.contains(&mdkg_run::projection::AGENTS_MD) {
            requested = requested.with(&[], &[mdkg_run::projection::AGENTS_MD]);
        }
    }
    let program = server_binary(choice.binary.as_deref())?;
    let server = mdkg_run::install::Server {
        command: program.to_str().unwrap_or_default(),
        ..DEFAULT_SERVER
    };
    for h in Harness::ALL.into_iter().filter(|h| requested.hooks(*h)) {
        let canonical = Path::new(h.adapter().hook_layout().canonical);
        if let Some(entry) = non_regular
            .iter()
            .find(|entry| canonical.starts_with(entry))
        {
            return Err(GraphError::failure(
                Code::InstallUnmergeable,
                format!(
                    "{} cannot be projected: committed source {} is a symlink or gitlink, not a missing default",
                    canonical.display(),
                    entry.display()
                ),
            ));
        }
    }
    // missing_canonical also reuses opencode_plugin_upgrade for an exact
    // previous default, matching the canonical bytes projections publishes.
    let pending: Vec<_> = Harness::ALL
        .into_iter()
        .filter(|h| requested.hooks(*h))
        .filter_map(|h| {
            mdkg_run::install::hook_files::missing_canonical(source, server.command, h)
                .map(|(_, text)| (h, text))
        })
        .collect();
    let keep = |path: &Path| !professional || professional_keeps(top, path);
    let plan = mdkg_run::install::preview_listed_with_hooks(
        source,
        top,
        &server,
        &requested,
        &removing,
        &keep,
        &pending,
        Some(undo),
    )
    .map_err(graph_error)?;
    ignore_hook_files_first(top, &requested, professional, undo, &keep, true)?;
    if professional {
        preview_professional_ignores(top, &plan, &requested)?;
    }
    Ok(())
}

/// What install would do to the skill mirrors of the ref-mode clone at
/// `top`, from its graph cache `cache`, for the projections listed at the
/// tip and with the setup's own keep rule: the plan install itself runs,
/// for `skills validate` (f-adopt #15953).
pub(crate) fn skill_mirror_plan(
    top: &Path,
    cache: &Path,
) -> Result<mdkg_run::install::SkillPreview, GraphError> {
    let graph = mdkg_graph::graphref::resolve(top)
        .map_err(|e| crate::coord::graph_git::graph_error(e.code, e.message))?;
    let professional = setup::inferred(top, graph.as_ref()) == setup::Setup::Professional;
    let projections = mdkg_run::projection::Projections::read(cache).map_err(graph_error)?;
    let keep = |path: &Path| !professional || professional_keeps(top, path);
    mdkg_run::install::preview_skill_mirrors(cache, top, &projections, &keep).map_err(graph_error)
}

/// The professional setup leaves tracked, linked and unrepresentable
/// paths alone. Both setup preflight and the eventual sync use this rule.
fn professional_keeps(top: &Path, path: &Path) -> bool {
    let full_folder = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        && std::fs::read_dir(path).map_or(true, |mut d| d.next().is_some());
    !full_folder
        && !linked_on_the_way(top, path)
        && path
            .strip_prefix(top)
            .ok()
            .and_then(|r| r.to_str())
            .filter(|r| !r.contains('\\'))
            .and_then(mdkg_run::install::exclude_pattern)
            .is_some()
        && matches!(tracked(top, path), Ok(false))
}

/// The same prospective exclude lines for setup preview and the real sync.
fn projection_exclusions(
    report: &InstallReport,
    synced: &mdkg_run::projection::Projections,
) -> Vec<(String, String)> {
    let personal = mdkg_run::install::personal_hook_targets(synced);
    report
        .changes
        .iter()
        .filter(|c| match c.action {
            mdkg_run::install::Action::Created => true,
            mdkg_run::install::Action::Updated => !personal.contains(&c.path.as_str()),
            _ => false,
        })
        .filter_map(|c| mdkg_run::install::exclude_pattern(&c.path).map(|l| (c.path.clone(), l)))
        .collect()
}

/// A private, exclusively created file lets Git apply planned patterns
/// without touching this clone's excludes, ignore file or undo records.
struct IgnoreProbe(PathBuf);

impl Drop for IgnoreProbe {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

impl IgnoreProbe {
    fn new(lines: &[String]) -> Result<Self, GraphError> {
        use std::io::Write as _;
        let path =
            std::env::temp_dir().join(format!("mdkg-ignore-preview-{}", uuid::Uuid::now_v7()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|e| GraphError::failure(Code::IoGitFailed, e.to_string()))?;
        let probe = Self(path);
        for line in lines {
            writeln!(file, "{line}")
                .map_err(|e| GraphError::failure(Code::IoGitFailed, e.to_string()))?;
        }
        Ok(probe)
    }

    fn check(&self, top: &Path, path: &str) -> Result<(bool, Option<String>), GraphError> {
        let mut config = std::ffi::OsString::from("core.excludesFile=");
        config.push(&self.0);
        let mut input = path.as_bytes().to_vec();
        input.push(0);
        let out = git::command(top)
            .arg("-c")
            .arg(config)
            .args(["check-ignore", "-v", "-z", "--stdin"])
            .run(Some(&input))
            .map_err(|e| GraphError::failure(Code::IoGitFailed, e.to_string()))?;
        if !out.status.success() && out.status.code() != Some(1) {
            // Git cannot classify a path below a symlink. As with the
            // real check, refuse the projection with the plain-path hint.
            return Ok((false, None));
        }
        // Verbose -z output is source, line number, pattern, path. A
        // negation matches successfully too, but does not ignore the file.
        let fields: Vec<_> = out.stdout.split(|b| *b == 0).collect();
        let Some(pattern) = fields.get(2).filter(|p| !p.is_empty()) else {
            return Ok((false, None));
        };
        let ignored = !pattern.starts_with(b"!");
        let rule = (!ignored).then(|| {
            fields[..3]
                .iter()
                .map(|f| String::from_utf8_lossy(f))
                .collect::<Vec<_>>()
                .join(":")
        });
        Ok((ignored, rule))
    }
}

fn preview_professional_ignores(
    top: &Path,
    plan: &InstallReport,
    synced: &mdkg_run::projection::Projections,
) -> Result<(), GraphError> {
    let planned = projection_exclusions(plan, synced);
    if planned.is_empty() {
        return Ok(());
    }
    let lines: Vec<_> = planned.iter().map(|(_, line)| line.clone()).collect();
    let probe = IgnoreProbe::new(&lines)?;
    for (path, _) in planned {
        if std::fs::symlink_metadata(top.join(&path)).is_ok() {
            continue;
        }
        let (ignored, rule) = probe.check(top, &path)?;
        if !ignored {
            let rule = rule.map_or_else(String::new, |r| format!(" ({r} re-includes it)"));
            return Err(GraphError::failure(
                Code::InstallUnmergeable,
                format!(
                    "{path} would be written, but git wouldn't ignore it{rule}, so `git \
                     add -A` would take it"
                ),
            )
            .with_hint("remove that rule, or choose another setup, then run install again"));
        }
    }
    Ok(())
}

/// Ref mode's projections (orchestrator #12839, Q7 (a)): the list the
/// graph tip's config holds, read with the skills and the hooks'
/// canonical files from the graph cache, and written into the checkout.
/// `--enable` and `--disable`, and a canonical hook file the graph lacks,
/// are published to the graph ref first, in one compare-and-swap. The
/// professional setup writes only files the repository doesn't track,
/// each added to the common `info/exclude`, and the AGENTS.md block only
/// when this install enables it.
fn projections(
    top: &Path,
    actor: Option<String>,
    choice: &InstallArgs,
    setup: setup::Setup,
) -> Result<InstallReport, GraphError> {
    use mdkg_graph::refwrite::{Change, LockMode, Request};
    let (enable, disable) = projection_changes(choice)?;
    let program = server_binary(choice.binary.as_deref())?;
    let command = program.to_str().unwrap_or_default();
    let server = mdkg_run::install::Server {
        command,
        ..DEFAULT_SERVER
    };
    let bound = |e: Option<PathBuf>| {
        e.ok_or_else(|| {
            GraphError::failure(
                Code::GraphRefMissing,
                "the graph ref isn't readable after setup",
            )
        })
    };
    let cache = bound(crate::coord::graph_git::bind(top)?)?;
    let repo = Repo::open(&cache)?;
    let undo = install_undo_dir(&repo)?;
    if !(choice.dry_run && mdkg_run::install::records_stale(top, &undo)) {
        mdkg_run::install::check_records(top, &undo).map_err(graph_error)?;
    }
    // The turns are held here: repairs only for an install that writes
    // (#14282).
    if !choice.dry_run {
        mdkg_run::install::repair_records(top, &undo).map_err(graph_error)?;
    }
    // What the publish changes is read at one tip, which is its base: a
    // change published meanwhile is refused as stale, never overwritten.
    let graph = mdkg_graph::graphref::resolve(top)
        .map_err(|e| crate::coord::graph_git::graph_error(e.code, e.message))?
        .ok_or_else(|| GraphError::failure(Code::GraphRefMissing, "the graph ref isn't set"))?;
    let read_at = git_stdout(
        top,
        &[
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", graph.name),
        ],
    )
    .map(|s| s.trim().to_owned())
    .ok_or_else(|| GraphError::failure(Code::GraphRefMissing, "the graph ref has no commit"))?;
    let at_tip = |path: &str| git_stdout(top, &["show", &format!("{read_at}:{path}")]);
    let text = at_tip(mdkg_graph::config::CONFIG_PATH).ok_or_else(|| {
        GraphError::failure(
            Code::ConfigUnreadable,
            "can't read the graph's config at its tip",
        )
    })?;
    let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        GraphError::failure(Code::ConfigInvalid, format!("the graph's config: {e}"))
    })?;
    let listed = mdkg_run::projection::Projections::from_config(&parsed).map_err(graph_error)?;
    let next = listed.with(&enable, &disable);
    // Hooks are opt-in (G24): only those asked for, or installed here.
    let (mut opted, skipped) =
        mdkg_run::install::opted_in(&cache, top, Some(&undo), &next, choice.hooks, &enable);
    // The professional setup leaves a link alone, and a per-user hook file
    // is never written through one: that hook projection is left out
    // rather than failing the install (#13893). Disabling it leaves the
    // link alone too: the list change is published, and nothing is
    // removed through the link (#14098).
    let mut linked = Vec::new();
    // The disables asked for are published as asked; only the local
    // removal leaves a linked target alone (#14242).
    let mut removing = disable.clone();
    if setup == setup::Setup::Professional {
        for h in Harness::ALL {
            let l = h.adapter().hook_layout();
            let name =
                mdkg_run::projection::name(&format!("hooks:{}", h.name())).map_err(graph_error)?;
            if l.personal
                && (opted.hooks(h) || disable.contains(&name))
                && std::fs::symlink_metadata(top.join(l.target))
                    .is_ok_and(|m| m.file_type().is_symlink())
            {
                opted = opted.with(&[], &[name]);
                removing.retain(|d| *d != name);
                linked.push(l.target);
            }
        }
    }
    let mut changes = Vec::new();
    // A changed list is published; so is an unchanged one that the config
    // doesn't list yet, as checkout mode writes it, so `install --enable`
    // records the legacy default (review of f7a84daf).
    if (!enable.is_empty() || !disable.is_empty())
        && (next.names() != listed.names() || !listed.declared())
    {
        let new = mdkg_run::projection::set_in_config(&text, &next.names()).ok_or_else(|| {
            GraphError::failure(Code::ConfigInvalid, ".mdkg/config.json isn't a JSON object")
        })?;
        changes.push(Change::Put {
            path: mdkg_graph::config::CONFIG_PATH.into(),
            bytes: new.into_bytes(),
        });
    }
    for h in Harness::ALL {
        if opted.hooks(h)
            && let Some((path, text)) =
                mdkg_run::install::hook_files::missing_canonical(&cache, command, h)
            && (!at_tip_has(top, &read_at, &path)?
                || (h == Harness::OpenCode
                    && at_tip(&path)
                        .and_then(|old| mdkg_run::hooks::opencode_plugin_upgrade(&old))
                        .as_deref()
                        == Some(text.as_str())))
        {
            changes.push(Change::Put {
                path,
                bytes: text.into_bytes(),
            });
        }
    }
    // The professional setup leaves AGENTS.md alone unless asked now.
    let mut synced = opted;
    if setup == setup::Setup::Professional && !enable.contains(&mdkg_run::projection::AGENTS_MD) {
        synced = synced.with(&[], &[mdkg_run::projection::AGENTS_MD]);
    }
    let mut notes = Vec::new();
    if !skipped.is_empty() {
        notes.push(mdkg_run::install::skipped_note(&skipped));
    }
    if !linked.is_empty() {
        notes.push(format!(
            "left alone, since they're links: {}; install never writes a per-user hook file \
             through one",
            linked.join(", ")
        ));
    }
    let professional = setup == setup::Setup::Professional;
    // A file whose tracking can't be read counts as tracked: left alone.
    // Each path's answer is read once, so the plan the exclusions come
    // from and the write that follows it leave the same files alone.
    let answers = std::cell::RefCell::new(std::collections::HashMap::new());
    let keep = |path: &Path| {
        !professional
            || *answers
                .borrow_mut()
                .entry(path.to_path_buf())
                .or_insert_with(|| professional_keeps(top, path))
    };
    let sync = |dry_run: bool| {
        mdkg_run::install::install_listed(
            &cache,
            top,
            &server,
            &synced,
            &removing,
            &keep,
            dry_run,
            Some(&undo),
        )
        .map_err(graph_error)
    };
    let common = git::common_dir(top)
        .and_then(std::fs::canonicalize)
        .map_err(|e| GraphError::failure(Code::IoGitFailed, e.to_string()))?;
    let exclude_file = common.join("info").join("exclude");
    // A per-user hook file install found keeps its ignore state (#13444).
    let personal = mdkg_run::install::personal_hook_targets(&synced);
    let writes = |report: &InstallReport| projection_exclusions(report, &synced);
    // Preview the entire future sync with the canonical hook bytes this
    // publication will add. Parsing, keep() filtering, and shared landing
    // checks must be the same as the eventual sync, in every setup.
    let pending_hooks: Vec<(Harness, String)> = Harness::ALL
        .into_iter()
        .filter_map(|h| {
            changes.iter().find_map(|change| match change {
                Change::Put { path, bytes } if path == h.adapter().hook_layout().canonical => {
                    Some(String::from_utf8(bytes.clone()).map(|text| (h, text)))
                }
                _ => None,
            })
        })
        .collect::<Result<_, _>>()
        .map_err(|e| GraphError::failure(Code::InstallUnmergeable, e.to_string()))?;
    let plan = mdkg_run::install::preview_listed_with_hooks(
        &cache,
        top,
        &server,
        &synced,
        &removing,
        &keep,
        &pending_hooks,
        Some(&undo),
    )
    .map_err(graph_error)?;
    // The same read-only checks ran before setup's ref/config writes.
    // Dry runs must refuse too, without adding any real ignore lines.
    if professional {
        preview_professional_ignores(top, &plan, &synced)?;
    }
    // Real installs ignore hook files before writing them (#14098).
    // Previews validate the same patterns without publishing those lines.
    let first = ignore_hook_files_first(top, &synced, professional, &undo, &keep, choice.dry_run)?;
    if !changes.is_empty() {
        let paths: Vec<&str> = changes.iter().map(Change::path).collect();
        if choice.dry_run {
            notes.push(format!(
                "dry run: would publish {} to the graph ref",
                paths.join(", ")
            ));
        } else {
            let who = repo.actor(actor);
            let clone = repo
                .store()?
                .store_id()
                .map_err(crate::coord::store_error)?;
            notes.push(format!("published {} to the graph ref", paths.join(", ")));
            // A hook disabled in the list is marked off here before it's
            // published, so an interruption before the sync never leaves
            // its old hook looking opted in (#14242): every disable asked
            // for, a linked one left alone included, and while still bound,
            // so a failure here leaves the turns releasable (#14344).
            mdkg_run::install::mark_hooks_off(top, &undo, &disable).map_err(graph_error)?;
            drop(repo);
            // The writer takes the exclusive graph lock; the binding's
            // shared one would block it.
            crate::coord::graph_git::unbind();
            let published = (|| {
                let writer = graph
                    .writer(top, LockMode::Local)
                    .map_err(|e| crate::coord::graph_git::graph_error(e.code, e.message))?;
                let to = |e: mdkg_graph::refwrite::Error| {
                    crate::coord::graph_git::graph_error(e.code(), e.to_string())
                };
                writer
                    .write(&Request {
                        id: format!("install-{}", uuid::Uuid::now_v7()),
                        base: Some(writer.snapshot_at(&read_at).map_err(to)?),
                        changes,
                        message: "install projections".into(),
                        trailers: vec![("Mdkg-Actor".into(), who), ("Mdkg-Clone".into(), clone)],
                    })
                    .map_err(to)
            })();
            let rebound = crate::coord::graph_git::bind(top);
            let tip = published?.commit;
            if bound(rebound?)? != cache {
                return Err(GraphError::failure(
                    Code::GraphRefMissing,
                    "the graph cache moved during install",
                ));
            }
            // The publish checks only the paths it changes: a projection
            // list changed meanwhile, elsewhere, would sync a list the
            // graph no longer holds.
            let now = git_stdout(
                top,
                &[
                    "show",
                    &format!("{tip}:{}", mdkg_graph::config::CONFIG_PATH),
                ],
            )
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| mdkg_run::projection::Projections::from_config(&v).ok());
            if now.as_ref().map(|p| p.names()) != Some(next.names()) {
                return Err(GraphError::conflict(
                    Code::GraphWriteStale,
                    "the graph's projection list changed while install published to it",
                )
                .with_hint("run install again: it reads the list at the graph's new tip"));
            }
        }
    }
    let mut report = if choice.dry_run {
        plan
    } else if professional {
        let planned = writes(&sync(true)?);
        let before: Vec<(String, Option<Vec<u8>>)> = planned
            .iter()
            .map(|(p, l)| (l.clone(), std::fs::read(top.join(p)).ok()))
            .collect();
        let lines: Vec<String> = planned.iter().map(|(_, l)| l.clone()).collect();
        let added = mdkg_run::install::exclude_lines(top, &exclude_file, &lines, Some(&undo))
            .map_err(graph_error)?;
        match sync(false) {
            Ok(r) => r,
            Err(e) => {
                let untouched: Vec<String> = before
                    .into_iter()
                    .zip(&planned)
                    .filter(|((l, was), (p, _))| {
                        added.contains(l) && std::fs::read(top.join(p)).ok() == *was
                    })
                    .map(|((l, _), _)| l)
                    .collect();
                mdkg_run::install::unexclude_lines(top, &exclude_file, &untouched, Some(&undo))
                    .ok();
                return Err(e);
            }
        }
    } else {
        sync(false)?
    };
    if !choice.dry_run {
        notes.extend(ignore_hook_files(
            top,
            &synced,
            professional,
            &undo,
            &created(&report),
            &first,
        )?);
    }
    if professional && !choice.dry_run {
        // Anything the plan didn't foresee (it changed between the two).
        let lines: Vec<String> = writes(&report).into_iter().map(|(_, l)| l).collect();
        mdkg_run::install::exclude_lines(top, &exclude_file, &lines, Some(&undo))
            .map_err(graph_error)?;
        // A per-user file install found keeps its ignore state (#13893).
        let found: Vec<&str> = report
            .changes
            .iter()
            .filter(|c| {
                c.action == mdkg_run::install::Action::Updated
                    && personal.contains(&c.path.as_str())
            })
            .map(|c| c.path.as_str())
            .collect();
        if !found.is_empty() {
            notes.push(format!(
                "each file install wrote is in the common info/exclude, except {}, which it \
                 found there and leaves as git ignored it or not",
                found.join(", ")
            ));
        } else if report
            .changes
            .iter()
            .any(|c| c.action != mdkg_run::install::Action::Removed)
        {
            notes.push("each file install wrote is in the common info/exclude".into());
        }
    }
    notes.append(&mut report.notes);
    report.notes = notes;
    Ok(report)
}

/// G24: each hook file a sync of `synced` writes is ignored by default,
/// so a teammate without mdkg never inherits hooks that run it: in the
/// common `info/exclude` for the professional setup (no tracked file
/// changes), otherwise in this checkout's `.gitignore`, recorded in
/// `undo` like the files install writes, so `install --undo` puts it
/// back. A file git already tracks can't be ignored, so install says so
/// instead. A per-user hook file (Claude's `.claude/settings.local.json`)
/// install `created` goes in the common `info/exclude`, personal to the
/// clone, in either setup, its line owned as the professional ones are;
/// one it found keeps its ignore state (orchestrator #13444).
fn ignore_hook_files(
    top: &Path,
    synced: &mdkg_run::projection::Projections,
    professional: bool,
    undo: &Path,
    created: &[String],
    first: &[String],
) -> Result<Vec<String>, GraphError> {
    let mut notes = Vec::new();
    let common = git::common_dir(top)
        .and_then(std::fs::canonicalize)
        .map_err(|e| GraphError::failure(Code::IoGitFailed, e.to_string()))?;
    let exclude = common.join("info").join("exclude");
    for target in mdkg_run::install::personal_hook_targets(synced) {
        let path = top.join(target);
        // Git can't say for a path under a linked folder: then what it
        // ignores, below, tells.
        if std::fs::symlink_metadata(&path).is_err() {
            continue;
        }
        // A tracked one keeps its state, but its hooks would reach
        // teammates with the next commit: said, as for shared files
        // (#14098). The professional setup never writes a tracked file.
        if tracked(top, &path).unwrap_or(false) {
            if !professional {
                notes.push(format!(
                    "{target} is tracked, so mdkg's hook entries in it are committed with it, and \
                 teammates without mdkg would run them; untrack it, or keep mdkg's hooks out of it"
                ));
            }
            continue;
        }
        let Some(line) = mdkg_run::install::exclude_pattern(target) else {
            continue;
        };
        // Install's own file (created now, or by an earlier install that
        // stopped before this, as the record says) gets the line; one it
        // found keeps its ignore state, but a line already there (another
        // worktree's) is recorded as relied on, so that worktree's undo
        // leaves it (#13893).
        let ours = created.iter().any(|c| c == target)
            || mdkg_run::install::created_by_install(top, undo, target);
        let held = mdkg_run::install::excludes(&exclude, &line);
        if !ours && !held {
            continue;
        }
        let added = mdkg_run::install::exclude_lines(top, &exclude, &[line], Some(undo))
            .map_err(graph_error)?;
        // What git makes of it: a .gitignore rule can re-include the
        // file, and git never ignores one under a linked folder.
        if !git_ignores(top, target) {
            notes.push(format!(
                "{target} holds mdkg's hooks, but git doesn't ignore it (a .gitignore rule \
                 re-includes it, or it's under a link): keep it out of commits"
            ));
        } else if !added.is_empty() || first.iter().any(|f| f == target) {
            notes.push(format!("{target} is ignored, in .git/info/exclude"));
        }
    }
    let mut ignored = Vec::new();
    for target in mdkg_run::install::hook_targets(synced) {
        let path = top.join(target);
        if !path.exists() {
            continue;
        }
        if tracked(top, &path)? {
            notes.push(format!(
                "{target} is tracked, so mdkg's hook entries in it are committed with it, and \
                 teammates without mdkg would run them; untrack it, or keep mdkg's hooks out of it"
            ));
            continue;
        }
        if !professional {
            ignored.push(target);
            continue;
        }
        let added = !mdkg_run::install::exclude_lines(
            top,
            &common.join("info").join("exclude"),
            &[format!("/{target}")],
            Some(undo),
        )
        .map_err(graph_error)?
        .is_empty();
        // Git's own answer: a .gitignore negation beats the exclude line
        // (#14223).
        if !git_ignores(top, target) {
            notes.push(format!(
                "{target} holds mdkg's hooks, but git doesn't ignore it (a .gitignore rule \
                 re-includes it, or it's under a link): keep it out of commits"
            ));
        } else if added || first.iter().any(|f| f == target) {
            notes.push(format!("{target} is ignored, in .git/info/exclude"));
        }
    }
    let mut added =
        mdkg_run::install::ignore_files(top, &ignored, Some(undo)).map_err(graph_error)?;
    // Ignored first, before the sync: reported here, as one line each.
    for t in &ignored {
        if first.iter().any(|f| f == t) && !added.iter().any(|a| a == t) {
            added.push((*t).to_owned());
        }
    }
    // Git's own answer, as in the other setups (#14328): a rule elsewhere
    // (a nested .gitignore's negation) can re-include the file.
    for t in &ignored {
        if !git_ignores(top, t) {
            notes.push(format!(
                "{t} holds mdkg's hooks, but git doesn't ignore it (a .gitignore rule \
                 re-includes it, or it's under a link): keep it out of commits"
            ));
        } else if added.iter().any(|a| a == t) {
            notes.push(format!("{t} is ignored, in .gitignore"));
        }
    }
    Ok(notes)
}

/// Before a sync writes them: each hook file of `synced` the sync will
/// write is ignored first, as [`ignore_hook_files`] would ignore it after,
/// so a crash between the two leaves nothing or an ignored file, never one
/// `git add -A` takes (orchestrator, #14098). A shared hook file counts
/// whether it's there or not, unless git tracks it; a per-user one only
/// when it isn't there yet; neither when `writes` says the sync leaves it
/// alone. A line left after a failed write can hide the user's own
/// existing file until `install --undo` takes it out (#14319 D1 (a)). The
/// targets it added a line for, which [`ignore_hook_files`] reports.
/// A dry run validates the planned patterns read-only and returns the
/// prospective targets instead of changing ignore files or records.
fn ignore_hook_files_first(
    top: &Path,
    synced: &mdkg_run::projection::Projections,
    professional: bool,
    undo: &Path,
    writes: &dyn Fn(&Path) -> bool,
    dry_run: bool,
) -> Result<Vec<String>, GraphError> {
    let common = git::common_dir(top)
        .and_then(std::fs::canonicalize)
        .map_err(|e| GraphError::failure(Code::IoGitFailed, e.to_string()))?;
    let exclude = common.join("info").join("exclude");
    // A tracking probe that fails stops install before the sync, never
    // lets it write a file it couldn't ignore first (#14223).
    // A shared hook file the sync will write, there already or not, as
    // long as git doesn't track it (#14319 D1 (a)); a per-user one only
    // when it isn't there yet, since a found one keeps its ignore state
    // (#13444).
    let personal = mdkg_run::install::personal_hook_targets(synced);
    let to_come = |target: &str| -> Result<bool, GraphError> {
        let path = top.join(target);
        let absent = std::fs::symlink_metadata(&path).is_err();
        // A path the sync leaves alone (under a link in professional,
        // #13895) is no file of install's (#14492).
        Ok((absent || !personal.contains(&target)) && !tracked(top, &path)? && writes(&path))
    };
    let mut excluded = Vec::new();
    let mut ignored = Vec::new();
    for target in mdkg_run::install::hook_targets(synced)
        .into_iter()
        .chain(personal.iter().copied())
    {
        if !to_come(target)? {
            continue;
        }
        if professional || personal.contains(&target) {
            if let Some(line) = mdkg_run::install::exclude_pattern(target) {
                excluded.push((target, line));
            }
        } else {
            ignored.push(target);
        }
    }
    let pending: Vec<_> = excluded
        .iter()
        .map(|(target, line)| (*target, line.clone()))
        .chain(ignored.iter().map(|target| (*target, format!("/{target}"))))
        .collect();
    if !pending.is_empty() {
        let lines: Vec<_> = pending.iter().map(|(_, line)| line.clone()).collect();
        let probe = IgnoreProbe::new(&lines)?;
        for (target, _) in &pending {
            if std::fs::symlink_metadata(top.join(target)).is_err() {
                let (ignored, rule) = probe.check(top, target)?;
                if !ignored {
                    return Err(hook_ignore_refusal(target, rule));
                }
            }
        }
    }
    if dry_run {
        return Ok(pending
            .into_iter()
            .map(|(target, _)| target.to_owned())
            .collect());
    }
    let mut first = Vec::new();
    for (target, line) in excluded {
        if !mdkg_run::install::exclude_lines(top, &exclude, &[line], Some(undo))
            .map_err(graph_error)?
            .is_empty()
        {
            first.push(target.to_owned());
        }
    }
    first.extend(mdkg_run::install::ignore_files(top, &ignored, Some(undo)).map_err(graph_error)?);
    // Git's own answer decides, in every setup, before any hook file is
    // made (#14381 E): a new one git still wouldn't ignore (a negation
    // re-includes it) is refused, naming the rule; an existing one is
    // warned about after the sync.
    for target in mdkg_run::install::hook_targets(synced)
        .into_iter()
        .chain(personal.iter().copied())
    {
        let path = top.join(target);
        let new = std::fs::symlink_metadata(&path).is_err();
        if new && writes(&path) && !tracked(top, &path)? && !git_ignores(top, target) {
            return Err(hook_ignore_refusal(target, overriding_rule(top, target)));
        }
    }
    Ok(first)
}

fn hook_ignore_refusal(target: &str, rule: Option<String>) -> GraphError {
    match rule {
        None => GraphError::failure(
            Code::InstallUnmergeable,
            format!(
                "{target} would hold mdkg's hooks, but git can't say it would ignore \
                 it (a link on its way, or no rule matches), so `git add -A` might \
                 take it"
            ),
        )
        .with_hint(format!(
            "make {target} a plain path in the worktree (no link on its way), or \
             choose another setup, then run install again"
        )),
        Some(rule) => GraphError::failure(
            Code::InstallUnmergeable,
            format!(
                "{target} would hold mdkg's hooks, but git wouldn't ignore it ({rule} \
                 re-includes it), so `git add -A` would take it"
            ),
        )
        .with_hint(format!(
            "remove that rule (or the negation re-including {target}), or choose another \
             setup, then run install again"
        )),
    }
}

/// The rule git matches `path` against last, as `source:line:pattern`
/// (`git check-ignore -v -n`): for a path git doesn't ignore, the
/// negation that re-includes it, if any.
fn overriding_rule(top: &Path, path: &str) -> Option<String> {
    let out = git::command(top)
        .args(["check-ignore", "-v", "-n", "--", path])
        .run(None)
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).into_owned();
    let rule = line.split('\t').next()?.trim().to_owned();
    (!rule.is_empty() && rule != "::").then_some(rule)
}

/// Whether git ignores `path` in `top` as things are (`git check-ignore`,
/// which applies every rule and its negations); `false` when it can't say.
fn git_ignores(top: &Path, path: &str) -> bool {
    git::command(top)
        .args(["check-ignore", "-q", "--", path])
        .run(None)
        .is_ok_and(|o| o.status.success())
}

/// The paths `report` says install created.
fn created(report: &InstallReport) -> Vec<String> {
    report
        .changes
        .iter()
        .filter(|c| c.action == mdkg_run::install::Action::Created)
        .map(|c| c.path.clone())
        .collect()
}

/// Whether a folder or the file itself on the way from `top` to `path` is
/// a link (or can't be read): what's written there lands elsewhere.
fn linked_on_the_way(top: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(top) else {
        return true;
    };
    let mut at = top.to_path_buf();
    for part in rel.components() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => return true,
        }
    }
    false
}

/// Whether git tracks `path` in `top`'s index, compared without case: a
/// case-folding disk makes `readme.md` `README.md`.
fn tracked(top: &Path, path: &Path) -> Result<bool, GraphError> {
    let rel = path.strip_prefix(top).unwrap_or(path);
    // The filesystem may take another spelling for the same file than
    // git's case folding does (`ſ` for `s` on APFS, #14119): the name the
    // file is stored under is asked about too.
    match stored_name(top, rel) {
        Err(()) => return Ok(true),
        Ok(Some(stored)) if stored != rel && tracked_as(top, &stored)? => return Ok(true),
        Ok(_) => {}
    }
    tracked_as(top, rel)
}

/// The name `rel` is stored under in `top`, one component at a time, as
/// the folder lists it: the entry that's the same file (device and
/// inode). `None` when it's not there, or can't be read.
#[cfg(unix)]
/// `Err` when it can't be told (a read failed, or no entry is the file):
/// the caller then takes the file for tracked, never for untracked
/// (#14264).
#[cfg(unix)]
fn stored_name(top: &Path, rel: &Path) -> Result<Option<PathBuf>, ()> {
    use std::os::unix::fs::MetadataExt as _;
    let mut dir = top.to_path_buf();
    let mut stored = PathBuf::new();
    for part in rel.components() {
        let want = match std::fs::symlink_metadata(dir.join(part)) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(()),
        };
        let mut found = None;
        for entry in std::fs::read_dir(&dir).map_err(drop)? {
            let entry = entry.map_err(drop)?;
            let m = std::fs::symlink_metadata(entry.path()).map_err(drop)?;
            if m.dev() == want.dev() && m.ino() == want.ino() {
                found = Some(entry.file_name());
                break;
            }
        }
        let name = found.ok_or(())?;
        dir.push(&name);
        stored.push(&name);
    }
    Ok(Some(stored))
}

#[cfg(not(unix))]
fn stored_name(_top: &Path, _rel: &Path) -> Result<Option<PathBuf>, ()> {
    Ok(None)
}

fn tracked_as(top: &Path, rel: &Path) -> Result<bool, GraphError> {
    // Always without case: no setting is read (a failed read must never
    // make a tracked file look untracked, #13895), and on a case-sensitive
    // disk a case alias only leaves a file alone.
    let out = git::command(top)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(format!(":(literal,icase){}", rel.to_string_lossy()))
        .run(None)
        .map_err(|e| GraphError::failure(Code::IoGitFailed, e.to_string()))?;
    match out.status.code() {
        Some(0) => Ok(true),
        // `--error-unmatch`'s answer for a path the index doesn't hold.
        Some(1) => Ok(false),
        _ => Err(GraphError::failure(
            Code::IoGitFailed,
            format!(
                "can't tell whether git tracks {}: {}",
                rel.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        )),
    }
}

/// Whether the graph's commit `at` holds `path`: a failed read is an
/// error, never taken for absence, which would publish over the file.
fn at_tip_has(top: &Path, at: &str, path: &str) -> Result<bool, GraphError> {
    let out = git::command(top)
        .args(["ls-tree", "-z", "--name-only", at, "--", path])
        .run(None)
        .map_err(|e| GraphError::failure(Code::IoGitFailed, e.to_string()))?;
    if !out.status.success() {
        return Err(GraphError::failure(
            Code::IoGitFailed,
            format!(
                "can't read {path} at the graph's tip: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(!out.stdout.is_empty())
}

/// A git command's output in `top`, as it printed it; `None` when it fails.
fn git_stdout(top: &Path, args: &[&str]) -> Option<String> {
    git::command(top)
        .args(args)
        .run(None)
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
}

/// The move from checkout mode to the graph ref, with changesets still
/// open, is refused rather than drained in 0.7.1 (orchestrator #12831,
/// Q2): once the clone follows the graph ref, a branch merged into main
/// no longer settles them (#14076). A clone already in ref mode, or one
/// with no graph yet, has none to lose.
fn refuse_open_changesets(top: &Path, dry_run: bool) -> Result<Vec<String>, GraphError> {
    // Whatever this checkout holds: the store is the clone's, and a
    // checkout from before the graph still moves the clone (#14205).
    if !matches!(mdkg_graph::graphref::resolve(top), Ok(None)) {
        return Ok(Vec::new());
    }
    let path =
        git::store_path(top).map_err(|e| GraphError::failure(Code::IoGitFailed, e.to_string()))?;
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(GraphError::failure(Code::IoReadFailed, e.to_string())),
        Ok(_) => {}
    }
    // A dry run writes nothing: opening the store may upgrade it, and
    // observing journals settlement (#14205).
    if dry_run {
        return Ok(vec![format!(
            "dry run: the coordinator isn't read; the move is refused while changesets are open \
             (`{BIN} status` lists them)"
        )]);
    }
    // Opened as Repo::store() opens it (#14320, #14475).
    let mut store = crate::coord::store_for(top)?;
    // Settled ones (main holds them now) aren't open: main is read under
    // each graph-owned path, nested workspaces too, so content merged by
    // squash or rebase settles (#14205). With no main (R2) nothing
    // settles them, and the queue stands as recorded.
    let main = crate::coord::main_ref(top);
    if git::has_ref(top, &main) {
        let paths = setup::committed_graph_paths(top, &main)?
            .unwrap_or_else(|| vec![mdkg_graph::committed::DEFAULT_DIR.to_owned()]);
        for prefix in paths {
            store
                .observe_main(top, &main, &prefix)
                .map_err(crate::coord::store_error)?;
        }
    }
    let state = store.coordinator().map_err(crate::coord::store_error)?;
    let open: Vec<String> = state
        .settlement_queue()
        .iter()
        .map(|c| c.id.to_string())
        .collect();
    if open.is_empty() {
        return Ok(Vec::new());
    }
    Err(crate::coord::graph_git::graph_error(
        "graph.open_changesets",
        format!(
            "{} open changeset(s) ({}) settle only on main; moving to the graph ref now would \
             leave them behind",
            open.len(),
            open.join(", ")
        ),
    )
    .with_hint(format!(
        "merge or abandon them first (`{BIN} status` lists them; `{BIN} abandon --branch \
         <branch>` drops one), then run install again"
    )))
}

/// The turns install holds in this clone, opened in whichever mode it's
/// in: the graph cache in ref mode, the checkout's graph otherwise. A
/// checkout with no graph at all has no turns to hold.
fn held_for_install(top: &Path, actor: Option<String>) -> Result<Option<HeldTurns>, GraphError> {
    let graph_root = match crate::coord::graph_git::bind(top) {
        Ok(Some(cache)) => cache,
        Ok(None) if mdkg_graph::config::has_config(top) => top.to_path_buf(),
        Ok(None) => return Ok(None),
        // A clone already in ref mode whose graph can't be bound: its
        // turns can't be held, so install doesn't go ahead without them.
        Err(e) if matches!(mdkg_graph::graphref::resolve(top), Ok(Some(_))) => return Err(e),
        // Checkout mode: the graph ref isn't there yet (install creates
        // or adopts it), so there's nothing to hold.
        Err(_) => return Ok(None),
    };
    let repo = Repo::open(&graph_root)?;
    hold_turns(&repo, actor).map(Some)
}

/// The turn leases install holds and its marker, released when it's
/// dropped.
#[derive(Debug)]
struct HeldTurns {
    root: PathBuf,
    dir: PathBuf,
    holder: String,
    names: Vec<String>,
    /// Dropped after the leases are released (fields drop after `drop`).
    marker: Option<state::HeldMarker>,
}

impl Drop for HeldTurns {
    fn drop(&mut self) {
        for name in &self.names {
            // Serialize the comparison/removal with every new publisher.
            let lock = state::lock_lease_record(&self.dir, name);
            if lock.is_ok() {
                state::remove_lease_record(&self.dir, name, &self.holder);
            }
            let scope = turn::lease_scope(name);
            let _ = crate::coord::release_local(
                &self.root,
                Some(self.holder.clone()),
                crate::coord::ReleaseTarget::Named(&scope),
            );
        }
    }
}

/// Takes the turn lease `run:<agent>` of every agent that works in this
/// worktree, or in a folder holding it, and keeps it while install plans,
/// backs up and writes: a turn can't start meanwhile, and the agent can't
/// change the folders install writes in (audit, 2026-10-22; held, not just
/// checked, since the review of d0a0f84). An agent whose turn is running
/// refuses install. The holder is `<actor>@install-<pid>`, with a lease
/// record as a launcher keeps one, so a turn says what holds it, and takes
/// the lease back if install died. Interactive registrations and active run
/// leases count too; a missing or unreadable location counts as here.
///
/// First, a marker for the worktree, which a turn checks once it holds its
/// lease, so an agent made or moved here meanwhile doesn't start either. A
/// lease whose holder is gone (a launcher or an install that died, or was
/// stopped with Ctrl-C) is taken back, as a turn takes it back (review of
/// c99eb3b).
fn hold_turns(repo: &Repo, actor: Option<String>) -> Result<HeldTurns, GraphError> {
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let mut held = HeldTurns {
        root: repo.root.clone(),
        dir: dir.clone(),
        holder: format!(
            "{}{}{}",
            repo.actor(actor),
            turn::INSTALL_HOLDER,
            std::process::id()
        ),
        names: Vec::new(),
        marker: None,
    };
    let top = std::fs::canonicalize(&repo.top).unwrap_or_else(|_| repo.top.clone());
    let marker = state::InstallMarker {
        record: turn::lease_record(held.holder.clone()),
        top: top.clone(),
        until_ms: state::now_ms() + state::MARKER_TTL_MS,
    };
    // Locked for as long as this process runs (sweeping dead ones first).
    held.marker = Some(state::mark_installing(&dir, &marker).map_err(|e| {
        graph_error(RunError::failure(
            "io.write_failed",
            format!("can't mark this worktree as installing: {e}"),
        ))
    })?);
    let here = install_turn_names(repo, &dir, &top)?;
    for name in here {
        let _record_lock = state::lock_lease_record(&dir, &name).map_err(graph_error)?;
        let scope = turn::lease_scope(&name);
        let root = &repo.root;
        // The holder recorded before the claim, as a launcher's turn does
        // (friction 81): an install killed in between is still named.
        let claimed = turn::claim_recorded(
            &dir,
            &name,
            &turn::lease_record(held.holder.clone()),
            || crate::coord::claim_report(root, Some(held.holder.clone()), &scope).map(|_| ()),
            |e: &GraphError| e.diagnostic.code == Code::CoordLeaseHeld,
            |holder| {
                crate::coord::holds_report(root, Some(holder.to_string()), &scope, None)
                    .ok()
                    .and_then(|r| r.data)
                    .is_none_or(|d| d.holds)
            },
            |holder| {
                let _ = crate::coord::release_local(
                    root,
                    Some(holder.to_string()),
                    crate::coord::ReleaseTarget::Named(&scope),
                );
            },
        )
        .map_err(graph_error)?;
        match claimed {
            Ok(()) => held.names.push(name.clone()),
            // Dropping `held` releases what it took.
            Err((e, records)) if e.diagnostic.code == Code::CoordLeaseHeld => {
                return Err(held_refusal(&name, &scope, records.first(), &e));
            }
            Err((e, _)) => return Err(e),
        }
    }
    Ok(held)
}

/// Called only after publishing the install marker. Existing holders must be
/// excluded by their run lease; newly registered actors see the marker after
/// claiming. Read routes without taking their locks: a dispatch can be waiting
/// under one, and install must refuse its run lease rather than wait for it.
fn install_turn_names(repo: &Repo, dir: &Path, top: &Path) -> Result<Vec<String>, GraphError> {
    let mut here = std::collections::BTreeMap::<String, bool>::new();
    let overlaps = |cwd: Option<&Path>| {
        cwd.map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()))
            .is_none_or(|cwd| cwd.starts_with(top) || top.starts_with(cwd))
    };
    let entries = std::fs::read_dir(dir)
        .map_err(|e| graph_error(RunError::io("can't list agent states for install", e)))?;
    for e in entries {
        let e =
            e.map_err(|e| graph_error(RunError::io("can't list agent states for install", e)))?;
        let file = e.file_name().to_string_lossy().into_owned();
        let Some(stem) = file.strip_suffix(".json") else {
            continue;
        };
        let name = stem.replace("%3A", ":");
        if state::check_name(&name).is_err() {
            continue;
        }
        let agent = std::fs::read(e.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<mdkg_run::AgentState>(&b).ok());
        here.insert(name, overlaps(agent.as_ref().map(|a| a.spec.cwd.as_path())));
    }
    let saved: std::collections::BTreeSet<_> = here.keys().cloned().collect();
    let common = git::common_dir(&repo.top)
        .map_err(|e| graph_error(RunError::io("can't find git common dir", e)))?;
    for name in mdkg_run::receiver::route::actors(&common) {
        let route = mdkg_run::receiver::route::read(&common, &name)
            .ok()
            .flatten();
        let overlap = overlaps(route.as_ref().and_then(|r| r.cwd.as_deref()));
        here.entry(name)
            .and_modify(|v| *v |= overlap)
            .or_insert(overlap);
    }
    // A claimed turn may not yet have a saved spec, registration or holder
    // file. The coordinator is authoritative; unknown locations fail closed.
    // A receiver's route may have moved since a queue was admitted, so its
    // current cwd cannot exclude an active holder's earlier dispatch. Keep
    // saved headless agents' established worktree filtering.
    let store = repo.store()?;
    let coord = store.coordinator().map_err(crate::coord::store_error)?;
    let now = state::now_ms();
    for lease in coord
        .leases()
        .filter(|l| l.confirmed && l.status == mdkg_coord::Status::Active && l.deadline > now)
    {
        if let Some(name) = lease.scope.as_str().strip_prefix("run:")
            && state::check_name(name).is_ok()
            && !saved.contains(name)
        {
            here.insert(name.to_string(), true);
        }
    }
    Ok(here
        .into_iter()
        .filter_map(|(name, here)| here.then_some(name))
        .collect())
}

/// Why install can't take an agent's turn lease, by who holds it.
fn held_refusal(
    name: &str,
    scope: &str,
    record: Option<&state::LeaseRecord>,
    e: &GraphError,
) -> GraphError {
    let (why, hint) = match record {
        Some(r) if r.holder.contains(turn::INSTALL_HOLDER) => (
            format!("another install ({}) is running in this worktree", r.holder),
            "wait until it's done, then run install again".to_string(),
        ),
        Some(r) => (
            format!(
                "agent `{name}`'s turn is running in this worktree ({} holds {scope})",
                r.holder
            ),
            format!(
                "wait until the turn ends (a `{BIN} agents up` between turns holds \
             nothing, and waits for install), then run install again"
            )
            .to_string(),
        ),
        None => (
            format!(
                "agent `{name}`'s turn lease is held: {}",
                e.diagnostic.message
            ),
            format!(
                "wait until {scope} is released (`{BIN} release {scope}` as its holder if \
                 that holder is gone), then run install again"
            ),
        ),
    };
    GraphError::conflict(
        Code::CoordLeaseHeld,
        format!("{why}: install changes nothing while it could change the same folders"),
    )
    .with_hint(hint)
}

/// Where install keeps its undo record for this worktree:
/// `<common>/mdkg/install/<id>`.
fn install_undo_dir(repo: &Repo) -> Result<PathBuf, GraphError> {
    undo_dir_of(&repo.top)
}

/// [`install_undo_dir`] for the checkout `top`.
fn undo_dir_of(top: &Path) -> Result<PathBuf, GraphError> {
    let common = git::common_dir(top)
        .and_then(std::fs::canonicalize)
        .map_err(|e| {
            graph_error(RunError::failure(
                "io.read_failed",
                format!("can't find this repository's git folder: {e}"),
            ))
        })?;
    Ok(mdkg_run::install::undo_dir(
        &common.join("mdkg").join("install"),
        top,
    ))
}

/// The repository's worktrees, canonical (a bare repository's git folder
/// isn't one).
/// The repository's worktrees; fails closed: with none listed the folder
/// checks under the lease would be skipped.
fn worktrees(top: &Path) -> Result<Vec<PathBuf>, GraphError> {
    match git::worktrees(top) {
        Ok(w) if !w.is_empty() => Ok(w),
        Ok(_) => Err(graph_error(RunError::failure(
            "run.cwd_outside",
            "git lists no worktrees for this repository",
        ))),
        Err(e) => Err(graph_error(RunError::failure(
            "run.cwd_outside",
            format!("can't list this repository's worktrees: {e}"),
        ))),
    }
}

/// An agent's folder must be inside one of the repository's worktrees, so a
/// reviewer started from $HOME isn't confined to $HOME (orchestrator,
/// 2026-10-21); a worker's may not be the worktree the launcher runs from
/// (the graph worktree, where `--cwd` defaults to), checked before the
/// agent is saved rather than at its first mail (orchestrator, 2026-10-22).
/// A reviewer, which can't write, may run there.
fn check_cwd(repo: &Repo, agent: &mdkg_run::AgentState) -> Result<(), GraphError> {
    let cwd = &agent.spec.cwd;
    // From the common dir and file checks alone, never git run through the
    // folder's own `.git`, which an agent could have pointed at a FIFO.
    let common = git::common_dir(&repo.top).map_err(|_| {
        GraphError::usage(
            Code::RunCwdOutside,
            "can't find this repository's git folder",
        )
    })?;
    let pin = turn::find_pin(&common, cwd)
        .map_err(|e| e.with_hint("pass --cwd <a worktree of this repository>"))
        .map_err(graph_error)?;
    turn::verify_pin(&common, &pin).map_err(graph_error)?;
    if agent.spec.role == Role::Worker && repo.top.starts_with(&pin.top) {
        // The main checkout holds the graph worktree: refused too, as a
        // worktree that holds another.
        let why = if pin.top == repo.top {
            format!(
                "{} is the checkout mdkg runs from, and no worker may use it",
                pin.top.display()
            )
        } else {
            format!(
                "{} holds the checkout mdkg runs from ({}), and no worker may use it",
                pin.top.display(),
                repo.top.display()
            )
        };
        return Err(graph_error(
            RunError::usage("run.cwd_outside", why).with_hint(format!(
                "set worktree: `{BIN} agents edit {name} --worktree` \
                 gives the worker a worktree of its own",
                name = agent.spec.name
            )),
        ));
    }
    Ok(())
}

/// Whether a parsed `.mdkg/config.json` has no `projections` key, so
/// install writes [`mdkg_run::projection::LEGACY`] (H-M6). An unreadable
/// list is the config check's to report, not this.
pub(crate) fn legacy_projections(config: &serde_json::Value) -> bool {
    mdkg_run::projection::Projections::from_config(config).is_ok_and(|p| !p.declared())
}

/// The warning `install` and `doctor` give a config with no `projections`
/// key. It changes nothing: the default is still written in 0.7.2.
pub(crate) fn legacy_projections_warning() -> Diagnostic {
    Diagnostic::warning(
        Code::InstallLegacyProjections,
        format!(
            "{} has no `{}` key, so install uses the legacy default list ({}); \
             in 0.7.3 a config without the key lists none of them",
            mdkg_graph::config::CONFIG_PATH,
            mdkg_run::projection::KEY,
            mdkg_run::projection::LEGACY.join(", "),
        ),
    )
    .with_hint(format!(
        "to keep them, set the key to today's list with the committed-config writer, then \
         run install: {}. In checkout mode, run it where the committed config is read \
         (config set names that checkout); other checkouts see it once the graph's commit \
         settles on main. If the checkout where the config is read already lists \
         projections, leave it: it reaches this checkout when it settles",
        legacy_projections_command(),
    ))
}

/// The command the hint names (orchestrator #20070): `config set` publishes
/// on the graph ref in ref mode and commits where the committed config is
/// read in checkout mode, writes no projection file (so a setup's
/// AGENTS.md is left alone), and leaves no dirty config to freeze a later
/// graph write.
pub(crate) fn legacy_projections_command() -> String {
    format!(
        "`{BIN} config set {} '{}'`",
        mdkg_run::projection::KEY,
        serde_json::json!(mdkg_run::projection::LEGACY),
    )
}

/// Whether `--enable` or `--disable` names a projection: only then does
/// install write the key (`--enable ""` writes none).
fn lists_projections(list: Option<&str>) -> bool {
    list.is_some_and(|l| mdkg_run::projection::names(l).is_ok_and(|n| !n.is_empty()))
}

/// The config a finished install read: the graph ref's tip in ref mode,
/// the checkout's otherwise. `None` when it can't be read.
fn installed_config(root: &Path, choice: &InstallArgs) -> Option<serde_json::Value> {
    let text = if installs_ref_mode(root, choice) {
        let top = crate::coord::graph_git::git::toplevel(root).ok()?;
        let graph = mdkg_graph::graphref::resolve(&top).ok()??;
        git_stdout(
            &top,
            &[
                "show",
                &format!("{}:{}", graph.name, mdkg_graph::config::CONFIG_PATH),
            ],
        )?
    } else {
        std::fs::read_to_string(root.join(mdkg_graph::config::CONFIG_PATH)).ok()?
    };
    serde_json::from_str(&text).ok()
}

pub fn install(root: &Path, actor: Option<String>, choice: &InstallArgs) -> u8 {
    let json = choice.json;
    let mut r = install_report(root, actor, choice).map_or_else(Report::failed, Report::ok);
    // Only a plain install writes the legacy default: undo, the rollback to
    // checkout mode and a changed list (which writes the key) don't.
    if r.failure.is_none()
        && !choice.undo
        && !choice.checkout_mode
        && !lists_projections(choice.enable.as_deref())
        && !lists_projections(choice.disable.as_deref())
        && installed_config(root, choice).is_some_and(|c| legacy_projections(&c))
    {
        r.diagnostics.push(legacy_projections_warning());
    }
    // Paths and notes carry file names anyone may have chosen: no escape
    // sequence reaches the terminal (lease-guard, 2026-10-22).
    // Line breaks too: one could start a forged `note:` line (round 6).
    let safe = output::shown_path;
    output::emit("install", json, r, |d| {
        // A dry run says what it would do, not what it did (e2e UX).
        for c in &d.changes {
            use mdkg_run::install::Action;
            let action = match (c.action, choice.dry_run) {
                (Action::Created, false) => "Created",
                (Action::Updated, false) => "Updated",
                (Action::Removed, false) => "Removed",
                (Action::Created, true) => "Would create",
                (Action::Updated, true) => "Would update",
                (Action::Removed, true) => "Would remove",
                (Action::Unchanged, _) => "Unchanged",
            };
            println!("{action} {}", safe(&c.path));
        }
        for n in &d.notes {
            println!("note: {}", safe(n));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_no_reply_turn_names_no_reply_id() {
        assert_eq!(
            no_reply(1),
            "answered NO_REPLY to 1 message(s); the launcher sent no final reply"
        );
    }

    /// Row 15: checkout mode refuses a launcher on main; ref mode, which
    /// writes no checkout, doesn't (R18).
    #[test]
    fn only_checkout_mode_refuses_a_launcher_on_main() {
        let dir = std::env::temp_dir().join(format!("mdkg-main-root-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join(".mdkg")).expect("dir");
        let git = |args: &[&str]| {
            let out = launcher_git()
                .arg("-C")
                .arg(&dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
                .args(args)
                .output()
                .expect("git");
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(
            dir.join(".mdkg/config.json"),
            "{\"schema_version\": 3, \"format_version\": \"0.7\"}\n",
        )
        .expect("config");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "c"]);
        let repo = Repo::open(&dir).expect("open");
        let refused = refuse_main_root(&repo).expect_err("checkout mode on main");
        assert_eq!(refused.diagnostic.code.as_str(), "run.branch_refused");
        git(&["config", "mdkg.graph.ref", "refs/heads/mdkg-graph"]);
        refuse_main_root(&repo).expect("ref mode runs from main");
        // The turn's main ref in ref mode: the config value, else none
        // (#14254), never the environment's (R4), checked in a child process with
        // $MDKG_MAIN_REF set, so no other test shares that environment.
        let with_env = |want: &str| {
            let out = std::process::Command::new(std::env::current_exe().expect("exe"))
                .args([
                    "--exact",
                    "run::tests::ref_mode_main_ref_ignores_the_environment",
                    "--test-threads=1",
                ])
                .env("MDKG_MAIN_REF", "refs/heads/other")
                .env("MDKG_TEST_MAIN_REF_DIR", &dir)
                .env("MDKG_TEST_MAIN_REF_WANT", want)
                .output()
                .expect("child");
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success() && text.contains("1 passed"), "{out:?}");
        };
        assert_eq!(turn_main_ref(&dir).expect("main ref"), None);
        with_env("");
        git(&["config", "mdkg.mainRef", "refs/remotes/origin/trunk"]);
        assert_eq!(
            turn_main_ref(&dir).expect("main ref").as_deref(),
            Some("refs/remotes/origin/trunk")
        );
        with_env("refs/remotes/origin/trunk");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The child half of `only_checkout_mode_refuses_a_launcher_on_main`:
    /// with $MDKG_MAIN_REF set, ref mode's main ref is still the config's.
    /// Run alone it has no repository to read, and passes.
    #[test]
    fn ref_mode_main_ref_ignores_the_environment() {
        let Some(dir) = std::env::var_os("MDKG_TEST_MAIN_REF_DIR") else {
            return;
        };
        let want = std::env::var("MDKG_TEST_MAIN_REF_WANT").expect("want");
        assert_eq!(
            std::env::var("MDKG_MAIN_REF").as_deref(),
            Ok("refs/heads/other")
        );
        // An empty want: none.
        let want = Some(want).filter(|w| !w.is_empty());
        assert_eq!(turn_main_ref(Path::new(&dir)).expect("main ref"), want);
    }

    #[test]
    fn the_launchers_mail_calls_carry_both_pins() {
        let opts = TurnOptions {
            common_dir: Some(PathBuf::from("/repo/.git")),
            trusted_worktree: Some(PathBuf::from("/repo/.worktrees/graph")),
            ..TurnOptions::default()
        };
        let mail = cli_mail(Path::new("/repo/.worktrees/graph"), "l".into(), &opts);
        assert_eq!(
            mail.trusted_common.as_deref(),
            Some(Path::new("/repo/.git"))
        );
        assert_eq!(
            mail.trusted_worktree.as_deref(),
            Some(Path::new("/repo/.worktrees/graph"))
        );
    }
}
