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

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum HarnessArg {
    Claude,
    Codex,
    #[value(name = "opencode")]
    OpenCode,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RoleArg {
    Worker,
    Reviewer,
}

/// Resolved execution settings, internal to `agents up`.
#[derive(Debug)]
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
        harness: a.harness.map(|h| match h {
            HarnessArg::Claude => Harness::Claude,
            HarnessArg::Codex => Harness::Codex,
            HarnessArg::OpenCode => Harness::OpenCode,
        }),
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

pub(crate) fn options(
    a: &AgentArgs,
    mdkg: PathBuf,
    repo: &Repo,
) -> Result<TurnOptions, GraphError> {
    refuse_main_root(repo)?;
    let config = mdkg_run::orchestration::load_agent_config(&repo.root).map_err(graph_error)?;
    mdkg_run::orchestration::agent::check_execution(&config, &a.agent).map_err(graph_error)?;
    Ok(TurnOptions {
        names: Names::from_config(&config).map_err(graph_error)?,
        // A person's run bills as the agent's policy says too: its
        // subscription, and the key only where the config allows it.
        auth: mdkg_run::policy::Policy::for_agent(&config, &a.agent)
            .map_err(graph_error)?
            .auth,
        // The operator's standing instructions come on every turn, a
        // manual passes as well as automatic watches.
        instructions: crate::agents::instructions(
            &a.agent,
            config.get("agents").and_then(|g| g.get(&a.agent)),
        )?,
        main_ref: crate::coord::main_ref(&repo.top),
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
fn refuse_main_root(repo: &Repo) -> Result<(), GraphError> {
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

pub(crate) fn print_turn(r: &TurnReport) {
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
    let main = crate::coord::main_ref(&repo.top);
    // Init leaves .gitignore alone; worktree creation owns this opt-in.
    let planned =
        turn::plan_agent_worktree(&common, &a.agent, &main, &opts.names, &agent.spec.protect)
            .map_err(graph_error)?;
    if !planned.exists() {
        mdkg_run::install::worktree::ignore_worktrees(&repo.top, a.dry_run).map_err(graph_error)?;
    }
    let path = if a.dry_run {
        Ok(planned)
    } else {
        turn::agent_worktree(&common, &a.agent, &main, &opts.names)
    }
    .map_err(graph_error)?;
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

/// Prepares a headless agent for a manual pass or watch. A committed
/// definition replaces its saved runtime settings under the turn lease.
pub(crate) fn prepare_watch(
    root: &Path,
    actor: Option<String>,
    a: &AgentArgs,
    create: bool,
) -> Result<Watched, GraphError> {
    check_worktree_args(a)?;
    let repo = Repo::open(root)?;
    let launcher = repo.actor(actor);
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let existing = state::load(&dir, &a.agent).map_err(graph_error)?;
    let new = existing.is_none();
    if new && !create && !a.dry_run {
        return Err(graph_error(RunError::usage(
            "run.unknown_agent",
            "agent state changed during preparation; restart agents up",
        )));
    }
    let default_cwd = repo.top.clone();
    let mut agent = if a.definition {
        let initial =
            state::resolve(&a.agent, None, flags(a), &default_cwd).map_err(graph_error)?;
        match existing {
            Some(existing) => {
                state::resolve_definition(existing, initial.spec, a.fresh).map_err(graph_error)?
            }
            None => initial,
        }
    } else {
        state::resolve(&a.agent, existing, flags(a), &default_cwd).map_err(graph_error)?
    };
    let mut base_opts = options(a, server_binary(a.binary.as_deref())?, &repo)?;
    give_worktree(a, &repo, &mut agent, &mut base_opts)?;
    if !base_opts.planned_worktree {
        check_cwd(&repo, &agent)?;
    }
    if new && !a.dry_run {
        state::save(&dir, &agent).map_err(graph_error)?;
    }
    let mail = cli_mail(root, launcher, &base_opts);
    let opts = TurnOptions {
        flags: if a.definition || (new && a.dry_run) {
            SpecFlags {
                definition: Some(agent.spec.clone()),
                ..base_opts.flags.clone()
            }
        } else {
            SpecFlags {
                fresh: a.fresh,
                ..SpecFlags::default()
            }
        },
        ..base_opts
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

/// What `mdkg install` was asked to do.
pub struct InstallChoice<'a> {
    pub dry_run: bool,
    pub undo: bool,
    pub binary: Option<&'a Path>,
    /// Comma-separated projection names, as typed.
    pub enable: Option<&'a str>,
    pub disable: Option<&'a str>,
}

fn install_report(
    root: &Path,
    actor: Option<String>,
    choice: &InstallChoice<'_>,
) -> Result<InstallReport, GraphError> {
    let InstallChoice {
        dry_run,
        undo,
        binary,
        ..
    } = *choice;
    let list = |s: Option<&str>| s.map_or(Ok(Vec::new()), mdkg_run::projection::names);
    let enable = list(choice.enable).map_err(graph_error)?;
    let disable = list(choice.disable).map_err(graph_error)?;
    no_inherited_git_env()?;
    crate::doctor::require_git()?;
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
    let graph = mdkg_run::install::worktree::GraphPlan::inspect(
        &repo.top,
        &crate::coord::main_ref(&repo.top),
    )
    .map_err(graph_error)?;
    // Validate the entire projection plan before graph setup can create a
    // checkout or ignore rule. Dry-run planning publishes no projection files.
    let mut report = mdkg_run::install::install_changing(
        &repo.top,
        &server,
        &enable,
        &disable,
        true,
        Some(&dir),
    )
    .map_err(graph_error)?;
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
        report = mdkg_run::install::install_changing(
            &repo.top,
            &server,
            &enable,
            &disable,
            false,
            Some(&dir),
        )
        .map_err(graph_error)?;
    }
    report.notes.push(graph_note);
    Ok(report)
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

fn read_lease_record(path: &Path) -> Option<state::LeaseRecord> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
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
        let record_path = state::lease_record_path(&dir, &name);
        let claim = || crate::coord::claim_report(&repo.root, Some(held.holder.clone()), &scope);
        let mut claimed = claim();
        let record = read_lease_record(&record_path);
        if let (Err(e), Some(r)) = (&claimed, &record)
            && e.diagnostic.code == Code::CoordLeaseHeld
            && turn::holder_gone(r)
        {
            let _ = crate::coord::release_local(
                &repo.root,
                Some(r.holder.clone()),
                crate::coord::ReleaseTarget::Named(&scope),
            );
            claimed = claim();
        }
        match claimed {
            Ok(_) => {
                held.names.push(name.clone());
                let record = turn::lease_record(held.holder.clone());
                let _ = serde_json::to_vec(&record).map(|b| std::fs::write(&record_path, b));
            }
            // Dropping `held` releases what it took.
            Err(e) if e.diagnostic.code == Code::CoordLeaseHeld => {
                return Err(held_refusal(&name, &scope, record.as_ref(), &e));
            }
            Err(e) => return Err(e),
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
    let common = git::common_dir(&repo.top)
        .and_then(std::fs::canonicalize)
        .map_err(|e| {
            graph_error(RunError::failure(
                "io.read_failed",
                format!("can't find this repository's git folder: {e}"),
            ))
        })?;
    Ok(mdkg_run::install::undo_dir(
        &common.join("mdkg").join("install"),
        &repo.top,
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

pub fn install(root: &Path, actor: Option<String>, choice: InstallChoice<'_>, json: bool) -> u8 {
    let r = install_report(root, actor, &choice).map_or_else(Report::failed, Report::ok);
    // Paths and notes carry file names anyone may have chosen: no escape
    // sequence reaches the terminal (lease-guard, 2026-10-22).
    // Line breaks too: one could start a forged `note:` line (round 6).
    let safe = output::shown_path;
    output::emit("install", json, r, |d| {
        for c in &d.changes {
            println!("{:?} {}", c.action, safe(&c.path));
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
