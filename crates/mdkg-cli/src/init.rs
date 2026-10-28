//! `mdkg init`: starts a format 0.7 graph, ready for agents,
//! with the mdkg-workflow skill seeded by default.
//!
//! - `.mdkg/config.json`: `schema_version` 3, `format_version`, a new
//!   `graph_id`, the work location (local by default, or shared in git)
//!   and explicit orchestration and projection choices.
//! - `.mdkg/README.md`: the layers and exact integration opt-ins.
//! - `.mdkg/skills/mdkg-workflow/SKILL.md`: the canonical workflow skill.
//!   Native integrations remain opt-in through install and skills sync.
//!
//! `--graph-only` omits the skill. Init leaves `.gitignore` and harness
//! files alone; worktree creation adds its own ignore entry. Templates are
//! built in (`.mdkg/templates/` holds optional overrides).
//!
//! On a folder that has a graph, init writes nothing. On a 0.7 graph, it is
//! a no-op, unless the choices it's given conflict with the graph's. It
//! refuses a 0.6 graph (that's `migrate`), a config it can't read, and a
//! `.mdkg/` holding anything but init's own output. Each new file appears
//! whole (staged and synced, then renamed into place without replacing
//! anything), the config comes last, and one init at a time holds a lock on `.mdkg`. So an
//! init stopped halfway runs again over what it had written.

use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use clap::{Args, ValueEnum};
use mdkg_format::node::FormatVersion;
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::GraphError;
use mdkg_graph::config::{self, CONFIG_PATH};
use mdkg_run::install::at::{self, Folder, Links, Tree};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

use crate::output::{self, Report};

/// The binary's own name, for the next steps it prints.
use mdkg_format::program::BIN;

/// Where the default orchestration names put agents (lane 5).
pub const DEFAULT_BRANCH: &str = "agent/{agent}";
pub const DEFAULT_WORKTREE: &str = ".worktrees/{agent}";

/// The placeholder both patterns must hold.
use mdkg_run::orchestration::{AGENT, branch_problem, worktree_problem};

/// The one skill init seeds, as the binary ships it.
pub const WORKFLOW_SLUG: &str = "mdkg-workflow";
pub const WORKFLOW_SKILL: &str = include_str!("workflow_skill.md");
pub const WORKFLOW_PATH: &str = ".mdkg/skills/mdkg-workflow/SKILL.md";
pub const README_PATH: &str = ".mdkg/README.md";
pub const INIT_README: &str = include_str!("init_readme.md");

/// The folders init stages files in.
const STAGED_IN: [&str; 3] = [".", ".mdkg", ".mdkg/skills"];
const STAGE_SUFFIX: &str = ".mdkg-init.tmp";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Work {
    /// Work stays on this machine, beside the coordinator store (default).
    #[default]
    Local,
    /// Work definitions are shared through git.
    Shared,
}

impl Work {
    /// The legacy alias for output consumers that still use it.
    pub fn committed(self) -> &'static str {
        match self {
            Self::Shared => "definitions",
            Self::Local => "none",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Local => "local",
        }
    }

    fn location(self) -> &'static str {
        match self {
            Self::Shared => "git",
            Self::Local => "local",
        }
    }

    fn legacy_profile(self) -> &'static str {
        match self {
            Self::Shared => "b",
            Self::Local => "c",
        }
    }
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Where work lives: local beside the store (default), or shared in git.
    /// Asked for when run in a terminal.
    #[arg(long, value_enum)]
    pub work: Option<Work>,
    /// The branch each agent works on; must hold `{agent}` (default
    /// `agent/{agent}`).
    #[arg(long, value_name = "PATTERN")]
    pub branch_pattern: Option<String>,
    /// Each agent's worktree, relative to the main checkout; must hold
    /// `{agent}` (default `.worktrees/{agent}`).
    #[arg(long, value_name = "PATTERN")]
    pub worktree_pattern: Option<String>,
    /// Write the config and setup README, without the mdkg-workflow skill.
    /// Integrations stay opt-in (`install --enable`).
    #[arg(long)]
    pub graph_only: bool,
    /// 0.6.0's spelling of the default (agent setup); kept for one release.
    #[arg(long, hide = true, conflicts_with = "graph_only")]
    pub agent: bool,
    /// Compatibility flag: init always leaves `.gitignore` alone.
    #[arg(long, hide = true)]
    pub no_gitignore: bool,
    /// Show what would change and write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

impl InitArgs {
    fn branch(&self) -> &str {
        self.branch_pattern.as_deref().unwrap_or(DEFAULT_BRANCH)
    }

    fn worktree(&self) -> &str {
        self.worktree_pattern.as_deref().unwrap_or(DEFAULT_WORKTREE)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileAction {
    Created,
    /// Compatibility action for files updated by earlier init builds.
    #[allow(dead_code)]
    Updated,
    /// Already as init would leave it.
    Kept,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InitFile {
    /// Relative to the root.
    pub path: String,
    pub action: FileAction,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Orchestration {
    pub branch: String,
    pub worktree: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InitData {
    /// The folder the graph was started in.
    pub root: String,
    /// The graph's UUID: a new v7, or the existing graph's.
    pub graph_id: String,
    /// `local` or `shared`, matching `init --work`.
    pub work: String,
    /// The canonical config value, `local` or `git`.
    pub work_location: String,
    /// Compatibility spelling: `b` or `c`; prefer `work`.
    pub profile: String,
    /// Compatibility spelling: `definitions` or `none`; prefer `work_location`.
    pub work_committed: String,
    pub orchestration: Orchestration,
    /// Whether the folder already had this 0.7 graph, so nothing was
    /// written.
    pub already: bool,
    /// Whether the agents' setup was written (not with `--graph-only`).
    pub agents: bool,
    /// The MCP command the agents' configs start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_command: Option<String>,
    /// Each file init wrote, or would write with `--dry-run` (named as
    /// install and skills sync name theirs).
    pub changes: Vec<InitFile>,
    pub dry_run: bool,
    /// Whether the folder is inside a git repository; coordination needs
    /// one.
    pub git: bool,
    /// Things init can't do for you.
    pub notes: Vec<String>,
    /// Commands to run next.
    pub next: Vec<String>,
}

pub fn run(root: &Path, args: &InitArgs) -> u8 {
    let report = init(root, args).unwrap_or_else(Report::failed);
    output::emit("init", args.json, report, |d| {
        if d.already {
            println!(
                "{} already has graph {} ({} work); nothing to do",
                output::shown_path(&d.root),
                d.graph_id,
                d.work
            );
            return;
        }
        let (wrote, updated) = if d.dry_run {
            ("would write", "would update")
        } else {
            ("wrote", "updated")
        };
        for f in &d.changes {
            let verb = match f.action {
                FileAction::Created => wrote,
                FileAction::Updated => updated,
                FileAction::Kept => "kept",
            };
            println!("{verb} {}", output::shown_path(&f.path));
        }
        println!(
            "{} graph {} ({} work: work.location {})",
            if d.dry_run { "would start" } else { "started" },
            d.graph_id,
            d.work,
            d.work_location
        );
        for n in &d.notes {
            println!("note: {}", output::shown_path(n));
        }
        if !d.next.is_empty() {
            println!("next:");
            for n in &d.next {
                println!("  {n}");
            }
        }
    })
}

fn init(root: &Path, args: &InitArgs) -> Result<Report<InitData>, GraphError> {
    init_observed(root, args, |_| {})
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InitPoint {
    Preflight,
    GuidePublished,
}

fn init_observed(
    root: &Path,
    args: &InitArgs,
    mut observe: impl FnMut(InitPoint),
) -> Result<Report<InitData>, GraphError> {
    // The launcher's own rules first, so a graph init writes is one it
    // runs, then init's stricter ones for what it writes.
    check_pattern("--branch-pattern", args.branch(), init_branch_problem)?;
    check_pattern("--worktree-pattern", args.worktree(), init_worktree_problem)?;
    if config::has_config(root) {
        return existing(root, args);
    }
    // One init at a time per folder, held until it's done: two at once
    // could publish each other's files. A stop releases it.
    let locked = if args.dry_run {
        None
    } else {
        let lock = lock(root)?;
        // Checked again now that no other init can be writing one.
        if config::has_config(root) {
            return existing(root, args);
        }
        Some(lock)
    };
    // What's here must be init's own before anything is touched. A
    // stopped init's staging files are left where they are: deleting
    // through a folder someone could swap for a link isn't worth it, and
    // they're named in a note.
    unexplained(root)?;
    observe(InitPoint::Preflight);
    let leftovers = own_leftovers(root);
    let work = match args.work {
        Some(p) => p,
        None if !args.json && !args.dry_run && interactive() => ask_work()?,
        None => Work::default(),
    };
    let graph_id = uuid::Uuid::now_v7().hyphenated().to_string();
    let git = in_git(root);
    let agents = !args.graph_only;
    let mut files = Vec::new();
    let mut notes: Vec<String> = leftovers
        .iter()
        .map(|l| format!("{l} is a stopped init's staging file, safe to delete"))
        .collect();

    // The skill first: what's at its path must be the shipped skill, as a
    // plain file, before anything else is written (checked whether it's
    // kept or new). A later opt-in can mirror it.
    if agents {
        let mut skill = new_file(root, WORKFLOW_PATH.into(), WORKFLOW_SKILL.into());
        if let Some((writer, _)) = &locked {
            skill.action = seed_file_at(writer, WORKFLOW_PATH, WORKFLOW_SKILL)?;
        }
        files.push(InitFile {
            path: skill.rel,
            action: skill.action,
        });
    }
    let mut readme = new_file(root, README_PATH.into(), INIT_README.into());
    if let Some((writer, _)) = &locked {
        readme.action = seed_file_at(writer, README_PATH, INIT_README)?;
    }
    files.push(InitFile {
        path: readme.rel,
        action: readme.action,
    });
    observe(InitPoint::GuidePublished);
    // Nothing outside `.mdkg` until the project enables it (opt-in
    // projections): the config lists none, and install writes what it
    // lists.
    notes.push(format!(
        "see {README_PATH} for setup: choose projections with `{BIN} install --enable`, \
         then `{BIN} skills sync` and `{BIN} agents up`; init leaves .gitignore alone"
    ));
    // Last, so an init stopped halfway leaves no config and can run again.
    let cfg = Planned {
        rel: CONFIG_PATH.into(),
        content: config_text(&graph_id, work, args),
        action: FileAction::Created,
    };
    if let Some((writer, _)) = &locked {
        apply(writer, &cfg)?;
    }
    files.push(InitFile {
        path: cfg.rel,
        action: cfg.action,
    });

    let mut warnings = Vec::new();
    if !git {
        warnings.push(
            Diagnostic::warning(
                Code::IoGitFailed,
                "this folder isn't in a git repository, so claims, leases and messages won't work",
            )
            .with_hint("run `git init`, then use the graph"),
        );
    }
    let mut next = vec![
        format!("{BIN} doctor"),
        format!("{BIN} new goal \"...\""),
        format!("{BIN} new task \"...\" --status todo"),
        format!("{BIN} next"),
    ];
    next.insert(1, format!("{BIN} install --enable <projections>"));
    // `new` needs the branch work settles on: its fix comes first.
    if let Some(p) = crate::doctor::main_branch_problem(root) {
        for (i, step) in p.fix.into_iter().enumerate() {
            next.insert(i, step);
        }
        warnings.push(Diagnostic::warning(Code::IoGitFailed, p.problem).with_hint(p.hint));
    }
    Ok(Report::ok(InitData {
        root: root.display().to_string(),
        graph_id,
        work: work.label().into(),
        work_location: work.location().into(),
        profile: work.legacy_profile().into(),
        work_committed: work.committed().into(),
        orchestration: Orchestration {
            branch: args.branch().into(),
            worktree: args.worktree().into(),
        },
        already: false,
        // No agent's file is written now: they're projections, enabled
        // with install.
        agents: false,
        mcp_command: None,
        changes: files,
        dry_run: args.dry_run,
        git,
        notes,
        next,
    })
    .with(warnings))
}

/// A folder that has a config: a no-op for the same 0.7 graph, and a
/// refusal for anything else. Nothing is written either way.
fn existing(root: &Path, args: &InitArgs) -> Result<Report<InitData>, GraphError> {
    let cfg = config::load(root).map_err(|mut e| {
        e.diagnostic.hint = Some(format!(
            "init leaves it as it is: fix it, or run `{BIN} doctor` to see what's wrong"
        ));
        e
    })?;
    if cfg.format == FormatVersion::Legacy {
        return Err(GraphError::usage(
            Code::InitNeedsMigrate,
            "this folder has an mdkg 0.6 graph, which init doesn't change",
        )
        .at(CONFIG_PATH, None)
        .with_hint(format!(
            "`{BIN} migrate --dry-run` plans its move to 0.7 (docs/migrating-from-0.6.md)"
        )));
    }
    let raw: Value = std::fs::read_to_string(root.join(CONFIG_PATH))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let text = |ptr: &str, default: &str| {
        raw.pointer(ptr)
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_string()
    };
    let branch = text("/orchestration/branch", DEFAULT_BRANCH);
    let worktree = text("/orchestration/worktree", DEFAULT_WORKTREE);
    let work = match cfg.work_committed {
        config::WorkCommitted::Definitions => Work::Shared,
        config::WorkCommitted::None => Work::Local,
    };
    // Only what was asked for explicitly can conflict.
    let mut conflicts = Vec::new();
    if args.work.is_some_and(|w| w != work) {
        conflicts.push("the work location (use `work localize` to move shared work out of git)");
    }
    if args.branch_pattern.as_deref().is_some_and(|b| b != branch) {
        conflicts.push("the branch pattern (edit orchestration.branch in the config)");
    }
    if args
        .worktree_pattern
        .as_deref()
        .is_some_and(|w| w != worktree)
    {
        conflicts.push("the worktree pattern (edit orchestration.worktree in the config)");
    }
    if !conflicts.is_empty() {
        return Err(GraphError::usage(
            Code::InitConflict,
            format!(
                "this folder's graph already chose {}, so init changes nothing",
                conflicts.join(", and ")
            ),
        )
        .at(CONFIG_PATH, None));
    }
    Ok(Report::ok(InitData {
        root: root.display().to_string(),
        graph_id: cfg.graph_id.map(|g| g.to_string()).unwrap_or_default(),
        work: work.label().into(),
        work_location: cfg.work_committed.location().into(),
        profile: work.legacy_profile().into(),
        work_committed: work.committed().into(),
        orchestration: Orchestration { branch, worktree },
        already: true,
        agents: false,
        mcp_command: None,
        changes: Vec::new(),
        dry_run: args.dry_run,
        git: in_git(root),
        notes: Vec::new(),
        next: vec![format!("{BIN} doctor")],
    }))
}

/// Refuses a `.mdkg/` that holds anything but init's own output: the
/// seeded skill exactly as the binary ships it, and staging files. A
/// stopped init's work is taken up again only when it's verifiably
/// init's; anything else might be someone's.
fn unexplained(root: &Path) -> Result<(), GraphError> {
    fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            found.push(rel(root, dir));
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            let shown = rel(root, &path);
            let kind = e.file_type();
            let name = e.file_name().to_string_lossy().into_owned();
            match kind {
                Ok(t) if t.is_dir() && !t.is_symlink() => walk(root, &path, found),
                Ok(t) if t.is_file() && is_stage_name(&name) && own_stage(root, &path) => {}
                Ok(t) if t.is_file() && shown == WORKFLOW_PATH => {
                    if !matches_generated(&path, WORKFLOW_SKILL) {
                        found.push(shown);
                    }
                }
                Ok(t) if t.is_file() && shown == README_PATH => {
                    if !matches_generated(&path, INIT_README) {
                        found.push(shown);
                    }
                }
                _ => found.push(shown),
            }
        }
    }
    let dir = root.join(".mdkg");
    match std::fs::symlink_metadata(&dir) {
        Err(_) => return Ok(()),
        Ok(m) if !m.is_dir() => {
            return Err(GraphError::usage(
                Code::InitUnexplained,
                ".mdkg is here but isn't a folder, so init leaves it alone",
            )
            .at(".mdkg", None));
        }
        Ok(_) => {}
    }
    let mut found = Vec::new();
    walk(root, &dir, &mut found);
    if found.is_empty() {
        return Ok(());
    }
    found.sort();
    let first = found[0].clone();
    Err(GraphError::usage(
        Code::InitUnexplained,
        format!(
            ".mdkg has {} file(s) init didn't write and no config, so init leaves it alone",
            found.len()
        ),
    )
    .at(first, None)
    .with_hint(format!(
        "move .mdkg aside to start fresh, or restore its config.json; `{BIN} migrate` takes a 0.6 graph"
    )))
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Asks where work lives on the terminal; an empty answer is local.
fn ask_work() -> Result<Work, GraphError> {
    let mut out = std::io::stdout();
    let _ = write!(
        out,
        "Where does work live?\n  local: work stays beside the local coordinator (default)\n  shared: work definitions and final states are committed to git\nWork [local/shared]: "
    );
    let _ = out.flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|e| {
        GraphError::failure(Code::IoReadFailed, format!("can't read the answer: {e}"))
    })?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "local" => Ok(Work::Local),
        "shared" => Ok(Work::Shared),
        _ => Err(
            GraphError::usage(Code::UsageInvalidArguments, "work is local or shared")
                .with_hint("run again and answer local or shared, or pass --work local|shared"),
        ),
    }
}

fn check_pattern(
    flag: &str,
    pattern: &str,
    problem: fn(&str) -> Option<&'static str>,
) -> Result<(), GraphError> {
    match problem(pattern) {
        // The pattern is typed text, so it isn't echoed in an error.
        Some(why) => Err(GraphError::usage(
            Code::UsageInvalidArguments,
            format!("{flag} {why}"),
        )),
        None => Ok(()),
    }
}

/// Why init won't write a branch pattern: the launcher's rules, then
/// mdkg's branches being ASCII (RELEASE_NOTES) with no `"` to escape.
fn init_branch_problem(pattern: &str) -> Option<&'static str> {
    branch_problem(pattern).or_else(|| {
        let sample = pattern.replace(AGENT, "agent");
        (!sample.is_ascii() || sample.contains('"')).then_some("must be ASCII, without \"")
    })
}

/// Why init won't write a worktree pattern: the launcher's rules, then a
/// plain path in the checkout or beside it: `..` only at the start, not
/// inside `.git` or `.mdkg`, no `~` or characters to escape in the config.
fn init_worktree_problem(pattern: &str) -> Option<&'static str> {
    worktree_problem(pattern).or_else(|| {
        if pattern.contains(['"', '\\']) {
            return Some("can't hold \" or \\");
        }
        if pattern.starts_with('~') {
            return Some("must be relative to the main checkout");
        }
        let parts: Vec<&str> = pattern.split('/').collect();
        // As written: path parsing drops a `.` part, the ignore line wouldn't.
        if parts.iter().any(|p| p.is_empty() || *p == ".") {
            return Some("can't hold empty or `.` parts");
        }
        let mut inner = parts.iter().skip_while(|p| **p == "..");
        if inner.clone().any(|p| *p == "..") || inner.next().is_none() {
            return Some("may start with `..` parts, but not hold them later");
        }
        if parts.first() == Some(&".git") || parts.first() == Some(&".mdkg") {
            return Some("can't be inside .git or .mdkg");
        }
        if mdkg_run::orchestration::leaves_project(pattern) {
            return Some("can't lead more than one folder above the main checkout");
        }
        None
    })
}

/// One file init writes.
struct Planned {
    rel: String,
    /// The whole file.
    content: String,
    action: FileAction,
}

fn new_file(root: &Path, rel: String, content: String) -> Planned {
    let action = if root.join(&rel).exists() {
        FileAction::Kept
    } else {
        FileAction::Created
    };
    Planned {
        rel,
        content,
        action,
    }
}

fn write_failed(rel: &str, e: &std::io::Error) -> GraphError {
    GraphError::failure(Code::IoWriteFailed, format!("can't write {rel}: {e}")).at(rel, None)
}

/// Locks `.mdkg` (an advisory lock on the folder, made if need be) for the
/// rest of this init. Another init holding it is a conflict.
fn lock(root: &Path) -> Result<(InitDir, std::fs::File), GraphError> {
    let writer = InitDir::open(root)?;
    let folder = std::fs::File::from(
        writer
            .graph
            .folder(Path::new("."), false, Links::Never)
            .map_err(|e| write_failed(".mdkg", &e))?
            .fd,
    );
    match folder.try_lock() {
        Ok(()) => Ok((writer, folder)),
        Err(std::fs::TryLockError::WouldBlock) => Err(GraphError::conflict(
            Code::InitExists,
            "another init is starting a graph here",
        )
        .with_hint("wait for it to finish, then run `doctor`")),
        Err(std::fs::TryLockError::Error(e)) => Err(write_failed(".mdkg", &e)),
    }
}

/// The checked project and graph handles. A changed graph name refuses;
/// every operation still uses the pinned handle if a swap races that check.
struct InitDir {
    project: Tree,
    graph: Tree,
}

impl InitDir {
    fn open(root: &Path) -> Result<Self, GraphError> {
        // `--root` can name a folder that doesn't exist yet. Create its
        // missing suffix from an existing directory's checked handle too.
        let base = root.ancestors().find(|p| p.is_dir()).ok_or_else(|| {
            GraphError::failure(Code::IoWriteFailed, "init has no existing parent folder")
        })?;
        let project = Tree::open(base)
            .and_then(|tree| tree.subtree(root.strip_prefix(base).unwrap_or(Path::new(".")), true))
            .map_err(|e| write_failed(".mdkg", &e))?;
        let graph = project
            .subtree(Path::new(".mdkg"), true)
            .map_err(|e| write_failed(".mdkg", &e))?;
        Ok(Self { project, graph })
    }

    fn check(&self) -> Result<(), GraphError> {
        let same = (|| -> std::io::Result<bool> {
            let named = self
                .project
                .folder(Path::new(".mdkg"), false, Links::Never)?;
            let held = self.graph.folder(Path::new("."), false, Links::Never)?;
            let named = rustix::fs::fstat(&named.fd)?;
            let held = rustix::fs::fstat(&held.fd)?;
            Ok(named.st_dev == held.st_dev && named.st_ino == held.st_ino)
        })();
        if same.unwrap_or(false) {
            Ok(())
        } else {
            Err(GraphError::conflict(
                Code::InitConflict,
                ".mdkg changed while init ran; nothing is written through its replacement",
            )
            .at(".mdkg", None))
        }
    }

    fn parent(&self, rel: &str, create: bool) -> Result<(Folder, OsString), GraphError> {
        self.check()?;
        let path = Path::new(rel).strip_prefix(".mdkg").map_err(|_| {
            GraphError::failure(Code::IoWriteFailed, "init writes only inside .mdkg")
        })?;
        let name = path
            .file_name()
            .ok_or_else(|| GraphError::failure(Code::IoWriteFailed, "init needs a file name"))?;
        let dir = self
            .graph
            .folder(
                path.parent().unwrap_or(Path::new(".")),
                create,
                Links::Never,
            )
            .map_err(|e| write_failed(rel, &e))?;
        Ok((dir, name.to_os_string()))
    }
}

/// Whether a file name is a staging file's: `.<target>.<run>.mdkg-init.tmp`.
fn is_stage_name(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(STAGE_SUFFIX)
}

/// The file a staging file was for: `SKILL.md` for
/// `.SKILL.md.<run>.mdkg-init.tmp`.
fn stage_target(name: &str) -> Option<&str> {
    let inner = name.strip_prefix('.')?.strip_suffix(STAGE_SUFFIX)?;
    Some(inner.rsplit_once('.').map_or(inner, |(t, _)| t))
}

/// Whether a staging file is provably one init wrote, by what it holds,
/// never by its name alone: empty (created, not yet written), or exactly
/// what init writes for its target.
/// - the seeded skill: a prefix of the shipped text;
/// - the config: the minimal 0.7 config, whole;
/// - README: a prefix of the shipped guide;
/// - legacy `.gitignore` stages remain recognizable, read-only.
pub(crate) fn own_stage(root: &Path, path: &Path) -> bool {
    let (Some(name), Some(dir)) = (path.file_name().and_then(|n| n.to_str()), path.parent()) else {
        return false;
    };
    let here = rel(root, dir);
    let target = match (here.as_str(), stage_target(name)) {
        // Staged a folder up, outside the skill (publish_new).
        (".mdkg/skills", Some("mdkg-workflow.SKILL.md")) => "skill",
        (".mdkg", Some("config.json")) => "config",
        (".mdkg", Some("README.md")) => "readme",
        ("", Some(".gitignore")) => "gitignore",
        // Init stages nothing else, so nothing else is init's.
        _ => return false,
    };
    let bytes = match target {
        "readme" => read_generated(path, INIT_README.len()),
        "skill" => read_generated(path, WORKFLOW_SKILL.len()),
        _ => std::fs::read(path).ok(),
    };
    let Some(bytes) = bytes else {
        return false;
    };
    // Created, not yet written.
    if bytes.is_empty() {
        return true;
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return false;
    };
    match target {
        "skill" => WORKFLOW_SKILL.starts_with(&text),
        "config" => config_prefix(&text),
        "readme" => INIT_README.starts_with(&text),
        _ => {
            let have = std::fs::read_to_string(root.join(".gitignore")).unwrap_or_default();
            let kept: std::collections::HashSet<&str> = have.lines().collect();
            text.lines().all(|l| {
                kept.contains(l)
                    || l.is_empty()
                    || l.starts_with("# mdkg: ")
                    || (l.starts_with('/') && l.ends_with('/') && !l.contains(char::is_whitespace))
            })
        }
    }
}

/// Whether text is the config as `config_text` writes it, or a prefix of
/// it (a stage cut off part way): its fixed text exactly, with only a
/// UUID, a work location and two patterns where those go. Old stopped
/// init stages using `committed` remain recognizable.
fn config_prefix(text: &str) -> bool {
    config_prefix_key(text, "location") || config_prefix_key(text, "committed")
}

fn config_prefix_key(text: &str, key: &str) -> bool {
    enum Piece {
        Fixed(String),
        Field(fn(char) -> bool),
    }
    fn uuid(c: char) -> bool {
        c.is_ascii_hexdigit() || c == '-'
    }
    fn word(c: char) -> bool {
        c.is_ascii_lowercase()
    }
    fn pattern(c: char) -> bool {
        c != '"' && c != '\\' && !c.is_control()
    }
    let pieces = [
        Piece::Fixed(format!(
            "{{\n  \"schema_version\": {},\n  \"format_version\": \"{}\",\n  \"graph_id\": \"",
            config::V07_SCHEMA_VERSION,
            config::FORMAT_VERSION_07
        )),
        Piece::Field(uuid),
        Piece::Fixed(format!("\",\n  \"work\": {{\n    \"{key}\": \"")),
        Piece::Field(word),
        Piece::Fixed("\"\n  },\n  \"orchestration\": {\n    \"branch\": \"".into()),
        Piece::Field(pattern),
        Piece::Fixed("\",\n    \"worktree\": \"".into()),
        Piece::Field(pattern),
        Piece::Fixed(format!(
            "\"\n  }},\n  \"{}\": []\n}}\n",
            mdkg_run::projection::KEY
        )),
    ];
    let mut rest = text;
    for piece in &pieces {
        match piece {
            Piece::Fixed(f) => {
                if rest.len() <= f.len() {
                    return f.starts_with(rest);
                }
                match rest.strip_prefix(f.as_str()) {
                    Some(r) => rest = r,
                    None => return false,
                }
            }
            Piece::Field(ok) => {
                let n = rest.find(|c: char| !ok(c)).unwrap_or(rest.len());
                rest = &rest[n..];
                if rest.is_empty() {
                    return true;
                }
            }
        }
    }
    rest.is_empty()
}

/// The staging files a stopped init left that are provably its own, to
/// name in a note. Only read: nothing is deleted.
fn own_leftovers(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for dir in STAGED_IN {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let plain = e.file_type().is_ok_and(|t| t.is_file());
            if plain && is_stage_name(&name) && own_stage(root, &e.path()) {
                found.push(rel(root, &e.path()));
            }
        }
    }
    found.sort();
    found
}

/// Writes a file's bytes beside it first, synced, so it's never seen half
/// written: `.<name>.<run>.mdkg-init.tmp`, a name of this run's own.
fn stage(dir: &Folder, name: &OsStr, rel: &str, content: &str) -> Result<OsString, GraphError> {
    // A name of this run's own, so no other file is ever replaced or
    // removed by staging.
    let run = uuid::Uuid::now_v7().simple().to_string();
    let tmp = OsString::from(format!(
        ".{}.{}{STAGE_SUFFIX}",
        name.to_string_lossy(),
        &run[20..]
    ));
    let mut f = at::create_file(&dir.fd, &tmp).map_err(|e| write_failed(rel, &e))?;
    f.write_all(content.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| {
            let _ = at::unlink(&dir.fd, &tmp);
            write_failed(rel, &e)
        })?;
    Ok(tmp)
}

/// Publishes the seeded skill. Something appearing at its path meanwhile
/// is fine only when it's the shipped skill, as a plain file; anything
/// else stops init before the agents' setup and the config, so it never
/// reports a skill it didn't write.
#[cfg(test)]
fn seed_skill(root: &Path) -> Result<FileAction, GraphError> {
    seed_file(root, WORKFLOW_PATH, WORKFLOW_SKILL)
}

/// Read bounded generated bytes without following a replacement link or
/// blocking on a special file that appeared after preflight.
fn read_generated(path: &Path, cap: usize) -> Option<Vec<u8>> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .ok()?;
    let file = std::fs::File::from(fd);
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= cap as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= cap).then_some(bytes)
}

fn matches_generated(path: &Path, content: &str) -> bool {
    read_generated(path, content.len()).as_deref() == Some(content.as_bytes())
}

#[cfg(test)]
fn seed_file(root: &Path, rel: &str, content: &str) -> Result<FileAction, GraphError> {
    seed_file_at(&InitDir::open(root)?, rel, content)
}

fn seed_file_at(writer: &InitDir, rel: &str, content: &str) -> Result<FileAction, GraphError> {
    if publish_at(writer, rel, content)? {
        return Ok(FileAction::Created);
    }
    let (dir, name) = writer.parent(rel, false)?;
    let matches = (|| -> std::io::Result<bool> {
        let f = at::open_file(&dir.fd, &name)?;
        if f.metadata()?.len() > content.len() as u64 {
            return Ok(false);
        }
        let mut bytes = Vec::new();
        f.take(content.len() as u64 + 1).read_to_end(&mut bytes)?;
        Ok(bytes == content.as_bytes())
    })();
    if matches.unwrap_or(false) {
        return Ok(FileAction::Kept);
    }
    Err(GraphError::conflict(
        Code::InitUnexplained,
        "something other than init's generated content appeared at its path while init ran",
    )
    .at(rel, None)
    .with_hint("move it aside, then run init again"))
}

/// Writes a new file at `rel` whole, as init writes its own: never
/// through a link on the way (each folder from the root is checked, and
/// made if missing), staged and synced, then renamed without replacing
/// anything. Skills stage a folder up; README stages inside `.mdkg`.
/// `false` when a file is already there.
pub(crate) fn publish_new(root: &Path, rel: &str, content: &str) -> Result<bool, GraphError> {
    publish_at(&InitDir::open(root)?, rel, content)
}

fn publish_at(writer: &InitDir, rel: &str, content: &str) -> Result<bool, GraphError> {
    let (target, name) = writer.parent(rel, true)?;
    if rustix::fs::statat(
        &target.fd,
        name.as_os_str(),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .is_ok()
    {
        return Ok(false);
    }
    // Staged a folder up (`.mdkg/skills/` for a skill's SKILL.md), so a
    // stop never leaves a stray file in the skill's own folder, where it
    // would be mirrored.
    let parent = Path::new(rel).parent().unwrap_or(Path::new(".mdkg"));
    let (folder, staged_as) = if rel == README_PATH {
        (
            writer
                .graph
                .folder(Path::new("."), false, Links::Never)
                .map_err(|e| write_failed(rel, &e))?,
            name.clone(),
        )
    } else {
        let staged_as = format!(
            "{}.{}",
            parent.file_name().unwrap_or_default().to_string_lossy(),
            name.to_string_lossy()
        );
        let dir = parent
            .parent()
            .unwrap_or(Path::new(".mdkg"))
            .strip_prefix(".mdkg")
            .map_err(|e| GraphError::failure(Code::IoWriteFailed, e.to_string()))?;
        (
            writer
                .graph
                .folder(dir, false, Links::Never)
                .map_err(|e| write_failed(rel, &e))?,
            OsString::from(staged_as),
        )
    };
    let tmp = stage(&folder, &staged_as, rel, content)?;
    if let Err(e) = writer.check() {
        let _ = at::unlink(&folder.fd, &tmp);
        return Err(e);
    }
    match at::rename_noreplace(&folder.fd, &tmp, &target.fd, &name) {
        Ok(()) => Ok(true),
        Err(e) => {
            // This run's own stage, just made: nobody else's file.
            let _ = at::unlink(&folder.fd, &tmp);
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(false)
            } else {
                Err(write_failed(rel, &e))
            }
        }
    }
}

fn apply(writer: &InitDir, p: &Planned) -> Result<(), GraphError> {
    match p.action {
        FileAction::Kept => Ok(()),
        FileAction::Updated | FileAction::Created => {
            let (dir, name) = writer.parent(&p.rel, true)?;
            let tmp = stage(&dir, &name, &p.rel, &p.content)?;
            // Published whole, and never over a file that appeared since
            // the plan was made.
            if let Err(e) = writer.check() {
                let _ = at::unlink(&dir.fd, &tmp);
                return Err(e);
            }
            let published = at::rename_noreplace(&dir.fd, &tmp, &dir.fd, &name);
            if published.is_err() {
                // This run's own stage, just made.
                let _ = at::unlink(&dir.fd, &tmp);
            }
            published.map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists && p.rel == CONFIG_PATH {
                    GraphError::usage(
                        Code::InitExists,
                        format!("{CONFIG_PATH} appeared while init ran"),
                    )
                    .at(CONFIG_PATH, None)
                } else {
                    write_failed(&p.rel, &e)
                }
            })
        }
    }
}

/// The new graph's config, exactly the consult's (section 2): identity,
/// format and the project's own choices. Every other setting is the
/// binary's default.
fn config_text(graph_id: &str, work: Work, args: &InitArgs) -> String {
    let v = json!({
        "schema_version": config::V07_SCHEMA_VERSION,
        "format_version": config::FORMAT_VERSION_07,
        "graph_id": graph_id,
        "work": {
            "location": work.location(),
        },
        "orchestration": {
            "branch": args.branch(),
            "worktree": args.worktree(),
        },
        // Opt in: nothing outside `.mdkg` until install enables it.
        mdkg_run::projection::KEY: [],
    });
    let mut text = serde_json::to_string_pretty(&v).unwrap_or_default();
    text.push('\n');
    text
}

/// Whether `root` is inside a git work tree (a `.git` here or above).
fn in_git(root: &Path) -> bool {
    root.ancestors().any(|d| d.join(".git").exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_init(words: &[&str]) -> InitArgs {
        use clap::Parser;
        #[derive(Parser)]
        struct ParserArgs {
            #[command(flatten)]
            init: InitArgs,
        }
        ParserArgs::try_parse_from(std::iter::once("init").chain(words.iter().copied()))
            .expect("valid init arguments")
            .init
    }

    #[test]
    fn work_choices_are_named_and_local_is_the_init_default() {
        use clap::ValueEnum;
        let args = parsed_init(&[]);
        assert_eq!(args.work.unwrap_or_default(), Work::Local);
        assert_eq!(parsed_init(&["--work", "local"]).work, Some(Work::Local));
        assert_eq!(parsed_init(&["--work", "shared"]).work, Some(Work::Shared));
        for old in ["b", "c", "none", "definitions", "git"] {
            assert!(Work::from_str(old, false).is_err());
        }
        #[derive(clap::Parser)]
        struct ParserArgs {
            #[command(flatten)]
            init: InitArgs,
        }
        use clap::Parser;
        assert!(ParserArgs::try_parse_from(["init", "--profile", "b"]).is_err());
    }

    #[test]
    fn init_writes_local_work_and_preserves_existing_config_aliases() {
        let root = scratch("local-default");
        let args = parsed_init(&["--json", "--no-gitignore"]);
        let data = init(&root, &args).unwrap().data.unwrap();
        assert_eq!(
            (data.work.as_str(), data.work_location.as_str()),
            ("local", "local")
        );
        let cfg: Value =
            serde_json::from_slice(&std::fs::read(root.join(CONFIG_PATH)).unwrap()).unwrap();
        assert_eq!(cfg["work"], json!({"location": "local"}));
        assert_eq!(cfg["projections"], json!([]));
        for (stored, choice) in [
            (json!({"committed":"none"}), "local"),
            (json!({"committed":"definitions"}), "shared"),
            (json!({"location":"git"}), "shared"),
        ] {
            let mut stored_cfg = cfg.clone();
            stored_cfg["work"] = stored;
            let bytes = serde_json::to_vec(&stored_cfg).unwrap();
            std::fs::write(root.join(CONFIG_PATH), &bytes).unwrap();
            let data = init(&root, &parsed_init(&["--json", "--work", choice]))
                .unwrap()
                .data
                .unwrap();
            assert!(data.already);
            assert_eq!(data.work, choice);
            assert_eq!(std::fs::read(root.join(CONFIG_PATH)).unwrap(), bytes);
            let other = if choice == "local" { "shared" } else { "local" };
            assert_eq!(
                init(&root, &parsed_init(&["--json", "--work", other]))
                    .unwrap_err()
                    .code(),
                Code::InitConflict
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_config_stage_is_init_s_only_if_it_is_a_prefix_of_its_shape() {
        let args = InitArgs {
            work: None,
            branch_pattern: Some("lanes/{agent}".into()),
            worktree_pattern: None,
            graph_only: false,
            agent: false,
            no_gitignore: false,
            dry_run: false,
            json: false,
        };
        let full = config_text("01a170d6-ab7e-70d7-a4e0-a2aa54d9aa0b", Work::Local, &args);
        for cut in 0..=full.len() {
            assert!(
                config_prefix(&full[..cut]),
                "prefix {cut}: {:?}",
                &full[..cut]
            );
        }
        let old = full.replace("\"location\": \"local\"", "\"committed\": \"none\"");
        assert_ne!(
            old, full,
            "the alias fixture must change the generated config"
        );
        for cut in 0..=old.len() {
            assert!(config_prefix(&old[..cut]), "legacy prefix {cut}");
        }
        assert!(!config_prefix("{\"half"));
        assert!(!config_prefix(&full.replace("\"work\"", "\"other\"")));
        assert!(!config_prefix(&format!("{full}extra")));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mdkg-init-unit-{name}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn init_creates_only_its_guide_config_and_optional_skill() {
        for work in ["local", "shared"] {
            for graph_only in [false, true] {
                let root = scratch("minimal");
                let mut words = vec!["--json", "--work", work];
                if graph_only {
                    words.push("--graph-only");
                }
                let data = init(&root, &parsed_init(&words))
                    .expect("init")
                    .data
                    .expect("data");
                assert!(data.changes.iter().all(|c| c.path.starts_with(".mdkg/")));
                let mut top = std::fs::read_dir(&root)
                    .expect("root")
                    .map(|e| e.expect("entry").file_name())
                    .collect::<Vec<_>>();
                top.sort();
                assert_eq!(top, [std::ffi::OsString::from(".mdkg")]);
                assert_eq!(
                    std::fs::read(root.join(README_PATH)).expect("guide"),
                    INIT_README.as_bytes()
                );
                assert_eq!(root.join(WORKFLOW_PATH).exists(), !graph_only);
                assert!(!root.join(".gitignore").exists());
                let cfg: Value =
                    serde_json::from_slice(&std::fs::read(root.join(CONFIG_PATH)).expect("config"))
                        .expect("JSON");
                assert_eq!(cfg["projections"], json!([]));
                assert_eq!(
                    cfg["orchestration"],
                    json!({"branch": DEFAULT_BRANCH, "worktree": DEFAULT_WORKTREE})
                );
                assert_eq!(
                    cfg["work"]["location"],
                    if work == "local" { "local" } else { "git" }
                );
                assert!(data.notes.iter().any(|n| n.contains(README_PATH)));
                let loaded = config::load(&root).expect("config");
                let receipt = mdkg_graph::validate::validate(&root, &loaded).expect("validate");
                assert!(receipt.errors.is_empty(), "{:?}", receipt.errors);
                std::fs::remove_dir_all(root).expect("clean fixture");
            }
        }
    }

    #[test]
    fn init_readme_resumes_only_its_own_published_or_staged_content() {
        let root = scratch("readme-resume");
        std::fs::create_dir_all(root.join(".mdkg")).expect("graph folder");
        let stage = root.join(".mdkg/.README.md.012345.mdkg-init.tmp");
        for part in ["", &INIT_README[..INIT_README.len() / 2], INIT_README] {
            std::fs::write(&stage, part).expect("stage");
            assert!(own_stage(&root, &stage));
        }
        std::fs::write(&stage, "another guide").expect("foreign stage");
        assert!(!own_stage(&root, &stage));
        std::fs::write(&stage, &INIT_README[..40]).expect("partial guide");
        std::fs::write(root.join(README_PATH), INIT_README).expect("published guide");
        let data = init(&root, &parsed_init(&["--json"]))
            .expect("resume")
            .data
            .expect("data");
        assert!(
            data.changes
                .iter()
                .any(|c| c.path == README_PATH && c.action == FileAction::Kept)
        );
        assert!(stage.is_file());
        assert!(data.notes.iter().any(|n| n.contains("staging file")));
        std::fs::remove_dir_all(root).expect("clean fixture");

        let root = scratch("foreign-readme");
        std::fs::create_dir_all(root.join(".mdkg")).expect("graph folder");
        std::fs::write(root.join(README_PATH), "user guide").expect("guide");
        let error = init(&root, &parsed_init(&["--json"])).expect_err("foreign guide");
        assert_eq!(error.code(), Code::InitUnexplained);
        assert_eq!(
            std::fs::read_to_string(root.join(README_PATH)).expect("guide"),
            "user guide"
        );
        assert!(!root.join(CONFIG_PATH).exists() && !root.join(WORKFLOW_PATH).exists());
        std::fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn init_dry_run_and_repeat_preserve_user_content() {
        let root = scratch("preserve");
        let ignore = b"target\n\xff\n";
        std::fs::write(root.join(".gitignore"), ignore).expect("ignore");
        std::fs::write(root.join("AGENTS.md"), "user instructions\n").expect("instructions");
        let dry = init(&root, &parsed_init(&["--dry-run", "--json"]))
            .expect("preview")
            .data
            .expect("data");
        assert!(dry.changes.iter().any(|c| c.path == README_PATH));
        assert!(!root.join(".mdkg").exists());
        init(&root, &parsed_init(&["--json"])).expect("init");
        assert_eq!(
            std::fs::read(root.join(".gitignore")).expect("ignore"),
            ignore
        );
        assert_eq!(
            std::fs::read_to_string(root.join("AGENTS.md")).expect("instructions"),
            "user instructions\n"
        );
        std::fs::write(root.join(README_PATH), "edited project guide\n").expect("edit guide");
        let config = std::fs::read(root.join(CONFIG_PATH)).expect("config");
        let again = init(&root, &parsed_init(&["--json"]))
            .expect("repeat")
            .data
            .expect("data");
        assert!(again.already && again.changes.is_empty());
        assert_eq!(
            std::fs::read(root.join(CONFIG_PATH)).expect("config"),
            config
        );
        assert_eq!(
            std::fs::read_to_string(root.join(README_PATH)).expect("guide"),
            "edited project guide\n"
        );
        std::fs::remove_dir_all(root).expect("clean fixture");
    }

    #[cfg(unix)]
    #[test]
    fn init_readme_late_collisions_and_links_remain_untouched() {
        let root = scratch("late-readme");
        std::fs::create_dir_all(root.join(".mdkg")).expect("graph folder");
        let path = root.join(README_PATH);
        std::fs::write(&path, "another writer's guide").expect("collision");
        assert_eq!(
            seed_file(&root, README_PATH, INIT_README)
                .expect_err("collision")
                .code(),
            Code::InitUnexplained
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("guide"),
            "another writer's guide"
        );
        std::fs::remove_file(&path).expect("clear collision");
        let outside = root.join("original.md");
        std::fs::write(&outside, INIT_README).expect("linked content");
        std::os::unix::fs::symlink(&outside, &path).expect("link");
        assert!(seed_file(&root, README_PATH, INIT_README).is_err());
        assert!(
            std::fs::symlink_metadata(&path)
                .expect("link kept")
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&outside).expect("original"),
            INIT_README
        );
        std::fs::remove_file(&path).expect("remove link");
        assert_eq!(
            seed_file(&root, README_PATH, INIT_README).expect("publish guide"),
            FileAction::Created
        );
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&path).expect("published").nlink(), 1);
        let stage = root.join(".mdkg/.README.md.012345.mdkg-init.tmp");
        std::fs::write(&stage, format!("{INIT_README}extra")).expect("oversized stage");
        assert!(!own_stage(&root, &stage));
        std::fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn init_readme_covers_every_projection_with_a_valid_enable_command() {
        use clap::Parser;
        for name in mdkg_run::projection::NAMES {
            assert!(
                INIT_README.contains(&format!("`mdkg install --enable {name}`")),
                "{name}"
            );
            crate::Cli::try_parse_from(["mdkg", "install", "--enable", name])
                .expect("documented projection command");
        }
        for words in [
            vec!["mdkg", "skills", "sync"],
            vec![
                "mdkg",
                "--root",
                ".worktrees/graph",
                "agents",
                "add",
                "worker",
                "--harness",
                "claude",
                "--role",
                "worker",
            ],
            vec!["mdkg", "install"],
            vec![
                "mdkg",
                "--root",
                ".worktrees/graph",
                "agents",
                "up",
                "worker",
            ],
            vec![
                "mdkg",
                "--root",
                ".worktrees/graph",
                "agents",
                "up",
                "worker",
                "--once",
            ],
        ] {
            crate::Cli::try_parse_from(words).expect("documented setup command");
        }
        assert_eq!(INIT_README.matches("```mermaid").count(), 2);
    }

    fn assert_directory_swap_refused(point: InitPoint) {
        for graph_only in [false, true] {
            let root = scratch("directory-swap");
            let outside = scratch("directory-swap-outside");
            let mut words = vec!["--json"];
            if graph_only {
                words.push("--graph-only");
            }
            let result = init_observed(&root, &parsed_init(&words), |at| {
                if at == point {
                    std::fs::rename(root.join(".mdkg"), root.join("held-graph"))
                        .expect("move checked graph");
                    std::os::unix::fs::symlink(&outside, root.join(".mdkg"))
                        .expect("replace graph with link");
                }
            });
            assert!(
                std::fs::read_dir(&outside)
                    .expect("outside")
                    .next()
                    .is_none(),
                "init must not write through the replacement graph link"
            );
            assert_eq!(
                result.expect_err("changed graph").code(),
                Code::InitConflict
            );
            assert!(!root.join("held-graph/config.json").exists());
            std::fs::remove_dir_all(root).expect("clean fixture");
            std::fs::remove_dir_all(outside).expect("clean outside");
        }
    }

    #[test]
    fn init_directory_swap_after_preflight_cannot_redirect_writes() {
        assert_directory_swap_refused(InitPoint::Preflight);
    }

    #[test]
    fn init_directory_swap_after_guide_publication_cannot_redirect_writes() {
        assert_directory_swap_refused(InitPoint::GuidePublished);
    }

    #[test]
    fn init_stages_publish_and_cleanup_stay_on_the_opened_directory() {
        // A swap after a check can't redirect even the individual operations.
        // Exercise all three against already opened graph and nested handles.
        for path in [".mdkg", ".mdkg/skills/mdkg-workflow"] {
            let root = scratch("pinned-publication");
            let outside = scratch("pinned-publication-outside");
            let writer = InitDir::open(&root).expect("checked graph");
            let rel = format!("{path}/SKILL.md");
            let (dir, name) = writer.parent(&rel, true).expect("checked parent");
            let held = root.join("held-directory");
            std::fs::rename(root.join(path), &held).expect("move parent");
            std::os::unix::fs::symlink(&outside, root.join(path)).expect("replace parent");
            let tmp = stage(&dir, &name, &rel, WORKFLOW_SKILL).expect("stage by handle");
            assert!(
                std::fs::read_dir(&outside)
                    .expect("outside")
                    .next()
                    .is_none()
            );
            at::rename_noreplace(&dir.fd, &tmp, &dir.fd, &name).expect("publish by handle");
            assert_eq!(
                std::fs::read(held.join(&name)).expect("published"),
                WORKFLOW_SKILL.as_bytes()
            );
            // A collision preserves the destination and cleans only our stage.
            let tmp = stage(&dir, &name, &rel, "replacement").expect("another stage");
            assert_eq!(
                at::rename_noreplace(&dir.fd, &tmp, &dir.fd, &name)
                    .expect_err("collision")
                    .kind(),
                std::io::ErrorKind::AlreadyExists
            );
            at::unlink(&dir.fd, &tmp).expect("cleanup by handle");
            assert!(!held.join(&tmp).exists());
            assert_eq!(
                std::fs::read(held.join(&name)).expect("preserved"),
                WORKFLOW_SKILL.as_bytes()
            );
            assert!(
                std::fs::read_dir(&outside)
                    .expect("outside")
                    .next()
                    .is_none()
            );
            std::fs::remove_dir_all(root).expect("clean fixture");
            std::fs::remove_dir_all(outside).expect("clean outside");
        }
    }

    #[test]
    fn init_creates_a_missing_root_from_a_checked_parent() {
        let parent = scratch("missing-root");
        let root = parent.join("new/project");
        init(&root, &parsed_init(&["--json"])).expect("init missing root");
        assert!(root.join(CONFIG_PATH).is_file());
        assert_eq!(
            std::fs::read(root.join(README_PATH)).expect("guide"),
            INIT_README.as_bytes()
        );
        std::fs::remove_dir_all(parent).expect("clean fixture");
    }

    #[test]
    fn a_collision_at_the_skill_path_stops_init_unless_it_is_the_skill() {
        // A folder where the skill goes: refused, nothing published.
        let dir = scratch("collide-dir");
        std::fs::create_dir_all(dir.join(WORKFLOW_PATH)).expect("mkdir");
        let e = seed_skill(&dir).expect_err("refused");
        assert_eq!(e.code(), Code::InitUnexplained);
        // Another file there: refused, and kept as it was.
        let dir = scratch("collide-file");
        std::fs::create_dir_all(dir.join(".mdkg/skills/mdkg-workflow")).expect("mkdir");
        std::fs::write(dir.join(WORKFLOW_PATH), "theirs").expect("write");
        assert!(seed_skill(&dir).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join(WORKFLOW_PATH)).expect("read"),
            "theirs"
        );
        // The shipped skill itself: fine.
        let dir = scratch("collide-same");
        std::fs::create_dir_all(dir.join(".mdkg/skills/mdkg-workflow")).expect("mkdir");
        std::fs::write(dir.join(WORKFLOW_PATH), WORKFLOW_SKILL).expect("write");
        assert_eq!(seed_skill(&dir).expect("kept"), FileAction::Kept);
        // Nothing there: published, with one name.
        let dir = scratch("seed");
        assert_eq!(seed_skill(&dir).expect("created"), FileAction::Created);
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(dir.join(WORKFLOW_PATH))
                .expect("meta")
                .nlink(),
            1
        );
    }

    #[test]
    fn the_seeded_skill_is_a_valid_skill() {
        let parsed = mdkg_format::frontmatter::parse(WORKFLOW_SKILL, WORKFLOW_PATH).expect("parse");
        let fm = parsed.frontmatter;
        assert_eq!(fm.get("name").and_then(|v| v.as_str()), Some(WORKFLOW_SLUG));
        assert!(fm.get("description").and_then(|v| v.as_str()).is_some());
    }
}
