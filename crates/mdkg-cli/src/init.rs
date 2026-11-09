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
use std::path::{Path, PathBuf};

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
/// Ref mode's "Start agents" section, in place of the checkout's.
const REF_START: &str = include_str!("init_readme_ref.md");

/// The guide ref mode commits to the graph ref: [`INIT_README`] with its
/// "Start agents" section (a graph checkout, merges into `main`) replaced
/// by ref mode's (publishes to the graph ref, no merge), and its skill
/// steps replaced: in ref mode install refreshes the mirrors from the
/// graph ref, `skills sync` refuses there, and `skills add` publishes to
/// the graph ref. It opens with a lead for agents, who find no `.mdkg`
/// folder in a ref-mode checkout to tell them the project uses mdkg.
pub fn ref_readme() -> String {
    const REPLACED: [(&str, &str); 4] = [
        (
            "# Project memory and agent setup\n\n",
            "# Project memory and agent setup\n\n\
             Agents: read `mdkg skills show mdkg-workflow` before any task (in a\n\
             launcher turn, `\"$MDKG_BIN\" skills show mdkg-workflow`). Never run\n\
             `mdkg init` in an existing clone: it would start a second graph.\n\n",
        ),
        ("\nmdkg skills sync\nmdkg doctor\n", "\nmdkg doctor\n"),
        (
            "Edit canonical skills in\n`.mdkg/skills/`, then run `mdkg skills sync` to refresh \
             the enabled mirrors.\n",
            "Canonical skills are in\n`.mdkg/skills/` on the graph ref, not in this checkout. \
             `mdkg install`\nrefreshes the enabled mirrors from the graph ref; `mdkg skills \
             sync` refuses\nhere.\n",
        ),
        (
            "With\n`--graph-only`, the skill is omitted; add it later with\n`mdkg skills add \
             mdkg-workflow`.\n",
            "With\n`--graph-only`, the skill is omitted; add it later with\n`mdkg skills add \
             mdkg-workflow`, which publishes it on the graph ref; once a skill\nmirror is \
             enabled (`mdkg install --enable skills:.agents/skills`),\n`mdkg install` \
             mirrors it.\n",
        ),
    ];
    let mut guide = INIT_README.to_owned();
    for (from, to) in REPLACED {
        assert!(guide.contains(from), "the guide has {from:?}");
        guide = guide.replacen(from, to, 1);
    }
    let start = guide
        .find("## Start agents\n")
        .expect("the guide has a Start agents section");
    let end = guide
        .find("## Work with the graph\n")
        .expect("the guide has a Work with the graph section");
    format!("{}{REF_START}\n{}", &guide[..start], &guide[end..])
}

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

#[derive(Debug, Clone, Args)]
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
    /// In ref mode (`mdkg.graph.ref` set), write `.mdkg/` into this
    /// checkout as 0.7.0 did, instead of creating the graph ref.
    #[arg(long)]
    pub in_checkout: bool,
    /// Start a new graph even though a fetched ref (any
    /// `refs/remotes/...` whose tip has a committed `.mdkg/config.json`)
    /// may hold this repository's graph, or git couldn't check. Without
    /// it, init refuses; for a graph it found, it names both ways to get
    /// it: merge the team's default branch (checkout mode) or `install`
    /// (ref mode).
    #[arg(long)]
    pub new_graph: bool,
    /// Start the graph straight on the graph ref (refs/heads/mdkg-graph) in
    /// this setup, as `install --setup` names them: `personal` (the graph
    /// syncs with origin), `professional` (with --graph-remote) or `local`
    /// (no remote). No branch gets a commit and the checkout is unchanged;
    /// nothing is pushed. Refused, before anything is written, when the
    /// clone already has a graph: at the checkout's root (`install --setup`
    /// moves it), staged but missing, nested below the root, in any other
    /// worktree of the clone, on the graph ref, or `mdkg.graph.ref` set
    /// already; and when a fetched branch sits at the setup's own graph
    /// destination.
    #[arg(long, value_enum, value_name = "SETUP", conflicts_with = "in_checkout")]
    pub setup: Option<crate::run::setup::Setup>,
    /// With --setup professional: the graph remote, an existing remote
    /// that shares no URL with another.
    #[arg(long, value_name = "REMOTE", requires = "setup")]
    pub graph_remote: Option<String>,
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
    /// In ref mode, the graph ref the files were committed to; the
    /// checkout is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_ref: Option<String>,
    /// With `init --setup`: the setup recorded (`personal`,
    /// `professional` or `local`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
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
            // A rerun can still finish or preview a step (the exclusion):
            // its notes say what changed or would.
            let rest = if d.notes.is_empty() {
                "; nothing to do"
            } else {
                ""
            };
            println!(
                "{} already has graph {} ({} work){rest}",
                output::shown_path(&d.root),
                d.graph_id,
                d.work
            );
            for note in &d.notes {
                println!("  {}", output::shown(note));
            }
            return;
        }
        let (wrote, updated) = match (d.dry_run, d.graph_ref.is_some()) {
            (true, false) => ("would write", "would update"),
            (false, false) => ("wrote", "updated"),
            (true, true) => ("would commit", "would commit"),
            (false, true) => ("committed", "committed"),
        };
        for f in &d.changes {
            let verb = match f.action {
                FileAction::Created => wrote,
                FileAction::Updated => updated,
                FileAction::Kept => "kept",
            };
            match &d.graph_ref {
                // A path on the graph ref, not in this checkout.
                Some(r) => println!("{verb} {} to {r}", f.path),
                None => println!("{verb} {}", output::shown_path(&f.path)),
            }
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
    init_observed(root, args, |_| {}).map_err(|e| layout_hint(root, args, e))
}

/// A layout refusal at a linked worktree (a graph checkout made by hand,
/// say) also names `init --setup`, which makes a ref-mode graph with no
/// worktree at all.
pub(crate) fn layout_hint(root: &Path, args: &InitArgs, mut e: GraphError) -> GraphError {
    if e.diagnostic.code != Code::InstallUnsupportedLayout || args.setup.is_some() {
        return e;
    }
    let dir = |flag: &str| {
        crate::coord::graph_git::git::command(root)
            .args(["rev-parse", "--path-format=absolute", flag])
            .run(None)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_owned())
    };
    let linked = matches!(
        (dir("--git-dir"), dir("--git-common-dir")),
        (Some(a), Some(b)) if a != b
    );
    if linked {
        let more = format!(
            "To start a graph on the graph ref without any worktree, run `{BIN} init --setup \
             <personal|professional|local>` at the primary checkout instead."
        );
        e.diagnostic.hint = Some(match e.diagnostic.hint.take() {
            Some(h) => format!("{h} {more}"),
            None => more,
        });
    }
    e
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
    if let Some(setup) = args.setup {
        return init_setup(root, args, setup);
    }
    // Ref mode: the graph is created on the graph ref, never in a checkout
    // (edd-01a18867, "Creating a graph without a checkout").
    if !args.in_checkout && in_git(root) {
        // Init may create `root`: ask git from the nearest folder that
        // exists, which is in the same repository.
        let probe = root.ancestors().find(|d| d.is_dir()).unwrap_or(root);
        let graph = mdkg_graph::graphref::resolve(probe)
            .map_err(|e| crate::coord::graph_git::graph_error(e.code, e.message))?;
        if let Some(graph) = graph {
            return init_ref(root, args, &graph);
        }
    }
    if config::has_config(root) {
        return existing(root, args);
    }
    refuse_fork(root, args)?;
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
        graph_ref: None,
        setup: None,
        dry_run: args.dry_run,
        git,
        notes,
        next,
    })
    .with(warnings))
}

/// `node edit`'s copies stay out of git in ref mode, as `install --setup`
/// leaves it (B18): adds the exclusion once, and a dry run says it would.
fn exclude_edit(top: &Path, dry_run: bool, notes: &mut Vec<String>) -> Result<(), GraphError> {
    use crate::run::setup;
    let common = crate::coord::graph_git::git::common_dir(top).map_err(crate::coord::git_error)?;
    let edit = setup::EDIT_IGNORE;
    let file = common.join("info/exclude");
    if dry_run {
        if !setup::excluded(top, edit)? {
            notes.push(format!("init would add {edit} to {}", file.display()));
        }
    } else if setup::exclude(top, &common, edit)? {
        notes.push(format!("{edit} added to {}", file.display()));
    }
    Ok(())
}

/// The keys `install --setup` may write, read before `init --setup` runs
/// it so a failure before the graph ref is published can put them back.
const SETUP_KEYS: [&str; 5] = [
    "mdkg.graph.ref",
    "mdkg.graph.remote",
    "mdkg.mainRef",
    "mdkg.follow.fetch",
    "mdkg.graph.publishDesign",
];

/// `init --setup`: the graph starts on the graph ref in that setup, with no
/// commit on any branch and the checkout unchanged (main-not-required).
/// Every refusal comes before anything is written. Then install's own
/// setup step records the setup's keys (`mdkg.graph.ref` last), and the
/// ref-mode init commits the graph to the new graph ref. The creation is
/// journalled under `mdkg.graph.ref`, so a stop between the two is finished
/// by `init`; a failure before the graph ref is published puts the keys
/// back as they were, leaving the clone in checkout mode.
fn init_setup(
    root: &Path,
    args: &InitArgs,
    setup: crate::run::setup::Setup,
) -> Result<Report<InitData>, GraphError> {
    use crate::coord::graph_git::graph_error;
    use crate::run::setup::{self as install_setup, GRAPH};
    let usage = |code: Code, message: String, hint: String| {
        GraphError::usage(code, message).with_hint(hint)
    };
    let name = setup.name();
    if !in_git(root) {
        return Err(usage(
            Code::InitConflict,
            format!("init --setup {name} needs a git repository: the graph goes on a graph ref"),
            "run `git init` first, or init without --setup".into(),
        ));
    }
    let top = crate::coord::graph_git::git::toplevel(root).map_err(crate::coord::git_error)?;
    let at_top = std::fs::canonicalize(root)
        .ok()
        .zip(std::fs::canonicalize(&top).ok())
        .is_some_and(|(a, b)| a == b);
    if !at_top {
        return Err(usage(
            Code::InitConflict,
            format!(
                "init --setup starts the repository's root graph, and {} isn't the repository's \
                 top folder",
                root.display()
            ),
            format!("run it at {}", top.display()),
        ));
    }
    let from_ref = |e: mdkg_graph::graphref::Error| graph_error(e.code, e.message);
    if let Some(graph) = mdkg_graph::graphref::resolve(&top).map_err(from_ref)? {
        return Err(usage(
            Code::InitConflict,
            format!(
                "this clone is in ref mode already (mdkg.graph.ref = {}), so init --setup has \
                 nothing to choose",
                graph.name
            ),
            format!(
                "`{BIN} init` (without --setup, and with the same --work and --graph-only \
                 options as before) creates or finishes the graph there; `{BIN} install --setup \
                 {name}` changes the setup"
            ),
        ));
    }
    let git_ok = |args: &[&str]| {
        crate::coord::graph_git::git::command(&top)
            .args(args)
            .run(None)
            .is_ok_and(|o| o.status.success())
    };
    let head_graph = git_ok(&["cat-file", "-e", &format!("HEAD:{CONFIG_PATH}")]);
    if config::has_config(&top) || head_graph {
        return Err(usage(
            Code::InitExists,
            format!(
                "this checkout already has a graph ({CONFIG_PATH}{}), so init --setup starts no \
                 second one",
                if head_graph { " on HEAD" } else { "" }
            ),
            format!("`{BIN} install --setup {name}` moves the checkout's graph to the graph ref"),
        ));
    }
    if git_ok(&[
        "rev-parse",
        "-q",
        "--verify",
        &format!("{GRAPH}^{{commit}}"),
    ]) {
        return Err(usage(
            Code::InitExists,
            format!("{GRAPH} exists already, so init --setup creates no graph ref"),
            format!(
                "`{BIN} install --setup {name}` adopts the graph on it; inspect it first with \
                 `git log {GRAPH}`"
            ),
        ));
    }
    // A graph below the root (checkout mode supports `sub/.mdkg`) would be
    // read from the new root graph once the whole clone is in ref mode
    // (Terra #20413). install --setup moves only a root graph, so the
    // hint doesn't send the user there (Opus #20438).
    let (nested, root_indexed) = checkout_graphs(&top)?;
    // A staged root config still on disk for lstat is a dangling link,
    // which restoring wouldn't bring back (Opus #20759): the next check
    // names it.
    let config_lstat = std::fs::symlink_metadata(top.join(CONFIG_PATH)).is_ok();
    if root_indexed && !config_lstat {
        // Staged, then removed from disk: committing alone wouldn't
        // bring the files back, and install --setup refuses their
        // deletion (Terra #20666).
        return Err(usage(
            Code::InitExists,
            format!(
                "this checkout already has a graph ({CONFIG_PATH} is staged but missing from \
                 the checkout), so init --setup starts no second one"
            ),
            // Only the deleted paths: a plain `git restore .mdkg` would
            // discard unstaged edits beside them (Terra #20704), and each
            // name is literal, never a glob (Terra #20738).
            format!(
                "restore only its deleted files with `git ls-files -d -z -- .mdkg | xargs -0 git \
                 --literal-pathspecs restore --`, commit the graph, and `{BIN} install --setup {name}` moves it to \
                 the graph ref; or unstage only the config with `git rm -q --cached \
                 {CONFIG_PATH}`"
            ),
        ));
    }
    if nested.iter().any(|p| p == CONFIG_PATH) {
        // The index may hold a regular config the link replaced on disk:
        // unstaging that would lose it, so it's restored instead (Terra
        // #20897). Only a staged link (mode 120000) is unstaged.
        let staged_link = root_indexed && {
            let out = crate::coord::graph_git::git::command(&top)
                .args(["ls-files", "-s", "-z", "--", CONFIG_PATH])
                .run(None)
                .map_err(|e| graph_error("io.git_failed", e.to_string()))?;
            out.status.success() && out.stdout.starts_with(b"120000 ")
        };
        return Err(usage(
            Code::InitExists,
            format!(
                "{CONFIG_PATH} is in the checkout but isn't a readable file (a dangling link?), \
                 so init --setup starts no graph beside it"
            ),
            // A staged link stays in the index once removed, and the
            // rerun would call it a staged graph (Terra #20882).
            if staged_link {
                format!(
                    "remove {CONFIG_PATH} and unstage it (`git rm -q --cached {CONFIG_PATH}`), \
                     or fix it, then run init --setup again"
                )
            } else if root_indexed {
                format!(
                    "the index holds a different {CONFIG_PATH}: put it back in place of the link \
                     with `git restore -- {CONFIG_PATH}` (inspect it first with `git show \
                     :{CONFIG_PATH}`), then run init --setup again"
                )
            } else {
                format!("remove or fix {CONFIG_PATH}, then run init --setup again")
            },
        ));
    }
    // Every other worktree shares the config init --setup changes, so a
    // graph anywhere in one would be left unread once the clone is in ref
    // mode (Terra #20633). Git lists one under an ignored folder only as
    // `dir/`, and one outside the checkout not at all, so each worktree
    // from the records gets the same scan as this one (Terra #20691).
    let here = std::fs::canonicalize(&top).unwrap_or_else(|_| top.clone());
    let others = crate::coord::graph_git::git::worktrees(&top)
        .map_err(|e| graph_error("io.git_failed", e.to_string()))?;
    for wt in &others {
        if std::fs::canonicalize(wt).unwrap_or_else(|_| wt.clone()) == here {
            continue;
        }
        let (found, _) = checkout_graphs(wt)?;
        let Some(first) = found
            .first()
            .cloned()
            .or_else(|| config::has_config(wt).then(|| CONFIG_PATH.to_owned()))
        else {
            continue;
        };
        let shown = shell_word(&wt.to_string_lossy());
        // The main checkout's git folder is the common one, whatever its
        // `.git` is (a file with --separate-git-dir); `git worktree
        // remove` takes only linked ones (Opus #20726).
        let primary = main_checkout(wt);
        return Err(usage(
            Code::InitExists,
            format!(
                "the {} {} has a graph ({first}), and ref mode applies to every worktree of the \
                 clone, so init --setup starts no second one",
                if primary {
                    "main checkout"
                } else {
                    "linked worktree"
                },
                wt.display()
            ),
            if primary {
                format!(
                    "run init --setup in the main checkout instead (`cd {shown}`): it says what \
                     to do with that graph"
                )
            } else {
                format!(
                    "init --setup changes the configuration every worktree shares: take the \
                     graph out of that worktree first (copy any uncommitted or ignored graph \
                     files somewhere safe: `git worktree remove {shown}` deletes them), or keep \
                     this clone in checkout mode"
                )
            },
        ));
    }
    if let Some(first) = nested.first() {
        return Err(usage(
            Code::InitExists,
            format!(
                "this checkout already has a graph at {first}, and ref mode applies to the whole \
                 clone, so init --setup starts no second one"
            ),
            "init --setup and install --setup move only a root `.mdkg`: keep the nested graph \
             in checkout mode, or move it to the repository's root first"
                .into(),
        ));
    }
    // The setup's own graph destination: a fetched branch there is the
    // team's graph (or something else), never one to start beside, even
    // with --new-graph; it honours a hand-set mdkg.graph.remoteRef (Terra
    // #20413).
    let remote = match setup {
        install_setup::Setup::Personal => Some("origin"),
        install_setup::Setup::Professional => args.graph_remote.as_deref(),
        install_setup::Setup::Local => None,
    };
    if let Some(remote) = remote {
        // The common config alone, as graph resolution reads it (Terra
        // #20453): a worktree's own config never masks it.
        let common = crate::coord::graph_git::git::common_dir(&top)
            .map_err(|e| graph_error("graph.io", e.to_string()))?;
        let file = common.join("config").to_string_lossy().into_owned();
        let branch = crate::coord::graph_git::git::command(&top)
            .args(["config", "--file", &file, "--get", "mdkg.graph.remoteRef"])
            .run(None)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_else(|| GRAPH.to_owned());
        let short = branch.strip_prefix("refs/heads/").unwrap_or(&branch);
        let fetched = format!("refs/remotes/{remote}/{short}");
        if git_ok(&[
            "rev-parse",
            "-q",
            "--verify",
            &format!("{fetched}^{{commit}}"),
        ]) {
            return Err(usage(
                Code::InitGraphFetched,
                format!(
                    "{fetched} exists: it's this {name} setup's own graph destination, so init \
                     --setup starts no new graph there"
                ),
                {
                    // Every name a hint shows is one shell word (Terra #20453).
                    let shown = shell_word(&fetched);
                    let setup_args = match args.graph_remote.as_deref() {
                        Some(r) if setup == install_setup::Setup::Professional => {
                            format!("{name} --graph-remote {}", shell_word(r))
                        }
                        _ => name.to_owned(),
                    };
                    if short == "mdkg-graph" {
                        format!(
                            "`{BIN} install --setup {setup_args}` adopts it (inspect it first \
                             with `git log {shown}`); a separate graph needs a setup whose graph \
                             remote holds none, such as `{BIN} init --setup local`"
                        )
                    } else {
                        // install --setup adopts only <remote>/mdkg-graph from
                        // checkout mode, never the branch remoteRef names.
                        format!(
                            "it's the branch mdkg.graph.remoteRef names, which install --setup \
                             doesn't adopt from checkout mode: inspect it with `git log {shown}`, \
                             and if it isn't this repository's graph, `git config --unset \
                             mdkg.graph.remoteRef` and run init --setup again"
                        )
                    }
                },
            ));
        }
    }
    refuse_fork(&top, args)?;
    let choice = |dry_run| install_setup::Choice {
        setup: Some(setup),
        graph_remote: args.graph_remote.as_deref(),
        publish_design: false,
        view: false,
        dry_run,
    };
    let no_projections = |_: &Path, _: &[PathBuf], _: &mdkg_run::projection::Projections, _| Ok(());
    // install's own checks (a professional setup's remote, origin for a
    // personal one), with nothing written.
    let preview = install_setup::setup(&top, &choice(true), no_projections)?;
    // A graph fetched from this setup's own graph remote would be adopted,
    // which is install's, not a new graph, even with --new-graph (#20348).
    if let Some(from) = &preview.adopts {
        return Err(usage(
            Code::InitGraphFetched,
            format!(
                "{from} holds a graph this {name} setup would adopt, so init --setup starts no \
                 new one"
            ),
            format!(
                "`{BIN} install --setup {name}` adopts it; a separate graph needs a setup whose \
                 graph remote holds none, such as `{BIN} init --setup local`"
            ),
        ));
    }
    // Setup adds an ignore rule in the common git folder. It applies to
    // every worktree, including those outside this checkout, so protect
    // visible edit files in each before adding it.
    if !install_setup::excluded(&top, install_setup::EDIT_IGNORE)? {
        for wt in &others {
            let untracked = crate::coord::graph_git::git::command(wt)
                .args([
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                    "-z",
                    "--",
                    ".mdkg-edit/",
                ])
                .run(None)
                .map_err(|e| graph_error("io.git_failed", e.to_string()))?;
            if !untracked.status.success() {
                return Err(graph_error(
                    "io.git_failed",
                    format!(
                        "can't check .mdkg-edit/ for untracked files in {}",
                        wt.display()
                    ),
                ));
            }
            if !untracked.stdout.is_empty() {
                return Err(usage(
                    Code::InitConflict,
                    format!("init --setup would hide existing untracked files in .mdkg-edit/ at {}", wt.display()),
                    "move those files somewhere safe first, or deliberately add `/.mdkg-edit/` to \
                     the clone's common info/exclude if they should stay ignored, then run init --setup again"
                        .into(),
                ));
            }
        }
    }
    // Use the graph writer's own identity policy, before setup writes keys,
    // the graph lock, exclusion, creation intent or coordinator store.
    if !args.dry_run {
        mdkg_graph::refwrite::check_commit_identity(&top)
            .map_err(|e| graph_error(e.code(), e.to_string()))?;
    }
    // The work choice comes first, so a prompt never runs after the keys
    // are written, and the preview shows what the run does (#20348).
    let work = match args.work {
        Some(w) => w,
        None if !args.json && !args.dry_run && interactive() => ask_work()?,
        None => Work::default(),
    };
    let mut args = args.clone();
    args.work = Some(work);
    let args = &args;
    let graph_id = uuid::Uuid::now_v7().hyphenated().to_string();
    if args.dry_run {
        let mut files = Vec::new();
        if !args.graph_only {
            files.push(WORKFLOW_PATH);
        }
        files.extend([README_PATH, CONFIG_PATH]);
        let mut notes: Vec<String> = preview
            .notes
            .into_iter()
            .filter(|n| !n.contains("there's no graph yet"))
            .collect();
        notes.push(format!(
            "dry run: init would commit the graph to {GRAPH}; this checkout stays unchanged"
        ));
        notes.push(
            "dry run: git's user identity for that commit is checked before setup writes on the \
             real run (`git config user.name` and `user.email`)"
                .into(),
        );
        return Ok(Report::ok(InitData {
            root: top.display().to_string(),
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
            agents: false,
            mcp_command: None,
            changes: files
                .into_iter()
                .map(|path| InitFile {
                    path: path.into(),
                    action: FileAction::Created,
                })
                .collect(),
            graph_ref: Some(GRAPH.into()),
            setup: Some(name.into()),
            dry_run: true,
            git: true,
            notes,
            next: vec![rerun(args, name)],
        }));
    }
    let common = crate::coord::graph_git::git::common_dir(&top)
        .map_err(|e| graph_error("graph.io", e.to_string()))?;
    let config_file = common.join("config").to_string_lossy().into_owned();
    let get = |key: &str| {
        crate::coord::graph_git::git::command(&top)
            .args(["config", "--file", &config_file, "--get", key])
            .run(None)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_owned())
    };
    let before: Vec<(&str, Option<String>)> = SETUP_KEYS.iter().map(|k| (*k, get(k))).collect();
    // The binding's shared graph lock would block setup's exclusive one.
    crate::coord::graph_git::unbind();
    let done = install_setup::setup(&top, &choice(false), no_projections);
    let done = match done {
        Ok(d) => d,
        Err(e) => return Err(restored(e, restore_keys(&top, &config_file, &before))),
    };
    let made = mdkg_graph::graphref::resolve(&top)
        .map_err(from_ref)
        .and_then(|g| {
            g.ok_or_else(|| graph_error("graph.ref_missing", "mdkg.graph.ref wasn't recorded"))
        })
        .and_then(|graph| init_ref(&top, args, &graph).map(|r| (graph, r)));
    match made {
        Ok((_, mut report)) => {
            if let Some(data) = report.data.as_mut() {
                data.setup = Some(name.into());
                // Setup's "no graph yet" line is out of date by now.
                let mut notes: Vec<String> = done
                    .notes
                    .into_iter()
                    .filter(|n| !n.contains("there's no graph yet"))
                    .collect();
                notes.append(&mut data.notes);
                data.notes = notes;
                data.next.insert(0, format!("{BIN} install"));
            }
            Ok(report)
        }
        Err(e) => {
            // Published already: the graph stands, and `init` finishes
            // what's left. Otherwise back to checkout mode, as it was.
            if !git_ok(&[
                "rev-parse",
                "-q",
                "--verify",
                &format!("{GRAPH}^{{commit}}"),
            ]) {
                if let Ok(Some(graph)) = mdkg_graph::graphref::resolve(&top) {
                    graph.abandon_creation(&top).ok();
                }
                return Err(restored(e, restore_keys(&top, &config_file, &before)));
            }
            Err(e)
        }
    }
}

/// Puts the setup keys back as `before` had them, after a failure that's
/// already being reported; returns the git commands that didn't take, so
/// the report names them (Terra #20413).
fn restore_keys(top: &Path, file: &str, before: &[(&str, Option<String>)]) -> Vec<String> {
    let mut failed = Vec::new();
    // `mdkg.graph.ref` last: the clone leaves ref mode only once the rest
    // is back.
    for (key, value) in before.iter().rev() {
        let cmd = crate::coord::graph_git::git::command(top);
        let (cmd, shown) = match value {
            Some(v) => (
                cmd.args(["config", "--file", file, key, v]),
                format!("git config {key} {}", shell_word(v)),
            ),
            None => (
                cmd.args(["config", "--file", file, "--unset-all", key]),
                format!("git config --unset-all {key}"),
            ),
        };
        // Unsetting a key that isn't set exits 5: already as it was.
        let ok = cmd
            .run(None)
            .is_ok_and(|o| o.status.success() || (value.is_none() && o.status.code() == Some(5)));
        if !ok {
            failed.push(shown);
        }
    }
    failed
}

/// `e`, with the restore commands that failed named in its hint: the
/// clone may still be in ref mode with a partial setup.
fn restored(mut e: GraphError, failed: Vec<String>) -> GraphError {
    if failed.is_empty() {
        return e;
    }
    let more = format!(
        "init --setup couldn't put the clone's setup back, so it may be in ref mode with no \
         graph: run {} by hand",
        failed
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", then ")
    );
    e.diagnostic.hint = Some(match e.diagnostic.hint.take() {
        Some(h) => format!("{h}. {more}"),
        None => more,
    });
    e
}

/// The `init --setup` command line `args` stand for, to run for real.
fn rerun(args: &InitArgs, name: &str) -> String {
    let mut words = vec![format!("{BIN} init --setup {name}")];
    if let Some(r) = &args.graph_remote {
        words.push(format!("--graph-remote {}", shell_word(r)));
    }
    if let Some(w) = args.work {
        words.push(format!("--work {}", w.label()));
    }
    if args.graph_only {
        words.push("--graph-only".into());
    }
    if let Some(p) = &args.branch_pattern {
        words.push(format!("--branch-pattern {}", shell_word(p)));
    }
    if let Some(p) = &args.worktree_pattern {
        words.push(format!("--worktree-pattern {}", shell_word(p)));
    }
    if args.new_graph {
        words.push("--new-graph".into());
    }
    words.join(" ")
}

/// Ref mode's init: the graph's first commit on an orphan graph ref (the
/// config with its identity, the setup README and, unless
/// `--graph-only`, the workflow skill; never `.mdkg/graph.json`),
/// validated strictly, then published by a create compare-and-swap. The
/// clone's identity record (`mdkg.graph.<graph_id>.ref`) follows. No file
/// appears in any checkout, and no code branch changes. A graph ref that
/// already exists is reported as the existing graph, and its identity
/// recorded when the clone has none yet.
fn init_ref(
    root: &Path,
    args: &InitArgs,
    graph: &mdkg_graph::graphref::GraphRef,
) -> Result<Report<InitData>, GraphError> {
    use crate::coord::graph_git::graph_error;
    use mdkg_graph::refwrite::{self, Change, LockKind, LockMode, Request};
    let from_ref = |e: mdkg_graph::graphref::Error| graph_error(e.code, e.message);
    let from_write = |e: refwrite::Error| graph_error(e.code(), e.to_string());
    // Nothing is written at `root` in ref mode, and it may not exist yet:
    // git runs from the nearest folder that does.
    let at = root.ancestors().find(|d| d.is_dir()).unwrap_or(root);
    let top = crate::coord::graph_git::git::toplevel(at).map_err(crate::coord::git_error)?;
    // A creation that didn't finish (a crash, a failed write) resumes with
    // its own id rather than minting another (g-core, Opus #8345).
    let pending = graph
        .pending_creation()
        .map_err(from_ref)?
        .filter(|(_, root, dir)| root.is_empty() && dir == ".mdkg")
        .map(|(id, _, _)| id);
    match mdkg_graph::graphref::tip(&top, graph) {
        Ok(tip) => {
            let authority = mdkg_graph::graphref::authority_at(&top, &tip).map_err(from_ref)?;
            let completing = pending.as_deref() == Some(authority.graph_id.as_str());
            if completing && !args.dry_run {
                // Published before a crash: record the identity now.
                graph.complete_creation(&top).map_err(from_ref)?;
            } else if !completing {
                // Recording a graph ref this init didn't create is
                // adoption, which is install's (`mdkg install`).
                graph
                    .check_identity(&authority.graph_id)
                    .map_err(from_ref)?;
            }
            let raw = authority.config.clone();
            let cfg = config::from_snapshot(authority.config)?;
            let mut report = existing_graph(root, args, cfg, &raw)?;
            // A retry after a crash between publication and the exclusion
            // finishes it.
            if let Some(data) = report.data.as_mut() {
                exclude_edit(&top, args.dry_run, &mut data.notes)?;
            }
            if completing
                && args.dry_run
                && let Some(data) = report.data.as_mut()
            {
                data.notes.push(format!(
                    "a creation of graph {} published before it finished; init would complete it \
                     (record this clone's identity)",
                    authority.graph_id
                ));
            }
            return Ok(report);
        }
        // A creation this clone started resumes; anything else would
        // fork a graph the clone fetched.
        Err(e) if e.code == "graph.ref_missing" => {
            if pending.is_none() {
                refuse_fork(root, args)?;
            }
        }
        Err(e) => return Err(from_ref(e)),
    }
    let work = match args.work {
        Some(p) => p,
        None if !args.json && !args.dry_run && interactive() => ask_work()?,
        None => Work::default(),
    };
    let graph_id = pending.unwrap_or_else(|| uuid::Uuid::now_v7().hyphenated().to_string());
    let mut planned: Vec<(&str, String)> = Vec::new();
    if !args.graph_only {
        planned.push((WORKFLOW_PATH, WORKFLOW_SKILL.into()));
    }
    planned.push((README_PATH, ref_readme()));
    planned.push((CONFIG_PATH, config_text(&graph_id, work, args)));
    let files: Vec<InitFile> = planned
        .iter()
        .map(|(path, _)| InitFile {
            path: (*path).into(),
            action: FileAction::Created,
        })
        .collect();
    if !args.dry_run {
        let repo = crate::coord::Repo::open(at)?;
        let actor = repo.actor(None);
        let clone = repo
            .store()?
            .store_id()
            .map_err(crate::coord::store_error)?;
        // The creator records a durable creation intent first, so a crash
        // anywhere below is finished by the next init.
        let writer = graph
            .creator(&top, LockMode::Local, &graph_id, "")
            .map_err(from_ref)?;
        let request = Request {
            id: format!("init-{graph_id}"),
            base: None,
            changes: planned
                .iter()
                .map(|(path, text)| Change::Put {
                    path: (*path).into(),
                    bytes: text.clone().into_bytes(),
                })
                .collect(),
            message: format!("init {graph_id}"),
            trailers: vec![("Mdkg-Actor".into(), actor), ("Mdkg-Clone".into(), clone)],
        };
        let lock = writer.lock(LockKind::Exclusive).map_err(from_write)?;
        let prepared = writer.prepare(&lock, &request).map_err(from_write)?;
        // Strictly valid before anything can read it as authority.
        let checked = mdkg_graph::committed::load_strict(&top, &prepared.commit, "", &graph_id);
        if let Err(e) = checked {
            writer.unpin(&lock, &prepared).ok();
            return Err(e);
        }
        writer.publish(&lock, &prepared).map_err(from_write)?;
        writer.unpin(&lock, &prepared).map_err(from_write)?;
        drop(lock);
        graph.complete_creation(&top).map_err(from_ref)?;
    }
    let mut notes = vec![format!(
        "the graph lives on {}, and this checkout is unchanged; read its setup guide with \
         `git show {}`",
        graph.name,
        shell_word(&format!("{}:{README_PATH}", graph.name))
    )];
    exclude_edit(&top, args.dry_run, &mut notes)?;
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
        agents: false,
        mcp_command: None,
        changes: files,
        graph_ref: Some(graph.name.clone()),
        setup: None,
        dry_run: args.dry_run,
        git: true,
        notes,
        next: vec![
            format!("{BIN} doctor"),
            format!("{BIN} new goal \"...\""),
            format!("{BIN} new task \"...\" --status todo"),
            format!("{BIN} next"),
        ],
    }))
}

/// What the clone's remote-tracking refs say about a graph it hasn't
/// adopted: none, one (`refs/remotes/...` whose tip has a committed
/// config, origin's `mdkg-graph` first), or git couldn't say.
pub(crate) enum Fetched {
    None,
    /// A fetched graph: `graph`, the ref a ref-mode clone would adopt
    /// (else the default branch), and `default`, a remote's default branch
    /// holding the graph, the branch a checkout-mode team merges.
    Graph {
        graph: String,
        default: Option<String>,
    },
    Unknown,
}

/// The ref personal setup adopts, the only one a hint names a command for.
const PERSONAL_REF: &str = "refs/remotes/origin/mdkg-graph";

impl Fetched {
    /// How to adopt it: personal setup for origin's `mdkg-graph`; for any
    /// other ref, which setup fits isn't certain, so install's help.
    fn adopt(refname: Option<&str>) -> String {
        if refname == Some(PERSONAL_REF) {
            format!("run `{BIN} install --setup personal`")
        } else {
            format!(
                "adopt it with `{BIN} install --setup <personal|professional>` (see `{BIN} install \
                 --help` for the setup whose remote and branch hold it)"
            )
        }
    }
}

/// `value` as one shell word: as it is when plain, else single-quoted, so
/// a ref or remote name pasted from a hint runs nothing.
fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:@%+=,-".contains(&b))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

/// The fetched graph at `root`'s repository: any remote-tracking ref whose
/// tip has a committed config, since any of them may be the graph this
/// clone should adopt, at the repository's top or at `root`. Only a folder
/// git places in no repository has
/// none; a repository git can't list or read, a ref name that isn't
/// UTF-8, or a tree that isn't local is [`Fetched::Unknown`], so init
/// fails closed. `ls-tree` reads trees only, so a config whose blob a
/// partial clone hasn't fetched still counts.
pub(crate) fn fetched_graph(root: &Path) -> Fetched {
    let Some(at) = root.ancestors().find(|d| d.is_dir()) else {
        return Fetched::None;
    };
    let git = |args: &[&str]| {
        crate::coord::graph_git::git::command(at)
            .args(args)
            .run(None)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout)
    };
    // With a `.git` here or above, a failed probe is a repository git
    // can't inspect, never the absence of one.
    if !in_git(at) && git(&["rev-parse", "--git-dir"]).is_none() {
        return Fetched::None;
    }
    let Some(listed) = git(&[
        "for-each-ref",
        "--format=%(refname)%00%(symref)",
        "refs/remotes",
    ])
    .and_then(|out| String::from_utf8(out).ok()) else {
        return Fetched::Unknown;
    };
    // A remote's symbolic `HEAD` (exactly `refs/remotes/<remote>/HEAD`)
    // names its default branch, listed on its own; any other ref, such as
    // a branch `feature/HEAD`, is probed.
    let mut refs = Vec::new();
    let mut defaults = Vec::new();
    for line in listed.lines().filter(|l| !l.is_empty()) {
        let Some((name, target)) = line.split_once('\0') else {
            return Fetched::Unknown;
        };
        let remote_head = name
            .strip_prefix("refs/remotes/")
            .and_then(|rest| rest.strip_suffix("/HEAD"))
            .is_some_and(|remote| !remote.is_empty() && !remote.contains('/'));
        if remote_head && !target.is_empty() {
            defaults.push(target);
        } else {
            refs.push(name);
        }
    }
    if refs.is_empty() {
        return Fetched::None;
    }
    // Default branches first, then personal setup's ref.
    refs.sort_by_key(|r| (!defaults.contains(r), *r != PERSONAL_REF));
    // The configs a second graph would collide with: the repository top's
    // and the selected root's (`services/api/.mdkg/config.json`), as
    // paths from the top.
    let Some(prefix) = git(&["rev-parse", "--show-prefix"])
        .and_then(|out| String::from_utf8(out).ok())
        .map(|p| p.trim_end_matches('\n').to_owned())
    else {
        return Fetched::Unknown;
    };
    let mut nested = prefix;
    for part in root.strip_prefix(at).unwrap_or(Path::new("")).components() {
        match part {
            std::path::Component::Normal(name) => match name.to_str() {
                Some(name) => {
                    nested.push_str(name);
                    nested.push('/');
                }
                None => return Fetched::Unknown,
            },
            std::path::Component::CurDir => {}
            _ => return Fetched::Unknown,
        }
    }
    let nested = format!("{nested}{CONFIG_PATH}");
    let mut paths = vec![CONFIG_PATH];
    if nested != CONFIG_PATH {
        paths.push(&nested);
    }
    // A default branch is the code a checkout-mode team merges only when
    // it holds code as well as the graph and isn't the configured graph
    // remote's ref: a graph remote whose host made `mdkg-graph` its
    // default branch is a graph ref like any other.
    let configured = mdkg_graph::graphref::resolve(at)
        .ok()
        .flatten()
        .and_then(|g| g.remote)
        .map(|r| {
            let branch = r
                .remote_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(&r.remote_ref);
            format!("refs/remotes/{}/{branch}", r.name)
        });
    let top = git(&["rev-parse", "--show-toplevel"])
        .and_then(|out| String::from_utf8(out).ok())
        .map(|t| PathBuf::from(t.trim_end_matches('\n')));
    let folders: Vec<String> = paths
        .iter()
        .filter_map(|p| p.strip_suffix("/config.json").map(str::to_owned))
        .collect();
    // The folders the ref's own graph owns (its config's workspaces and
    // subgraphs too), else the root's and the selected root's; a config
    // that can't be read proves no code.
    // A top-level config listed but unreadable (a blob a blobless clone
    // hasn't fetched) proves no code either.
    let code_default = |r: &str, top_config: bool| {
        configured.as_deref() != Some(r)
            && top.as_deref().is_some_and(|top| {
                let owned = match crate::run::setup::committed_graph_paths(top, r) {
                    Ok(Some(owned)) => owned,
                    Ok(None) if !top_config => folders.clone(),
                    Ok(None) | Err(_) => return false,
                };
                crate::run::setup::full_shape(top, r, &owned).unwrap_or(false)
            })
    };
    // Every default branch is checked before any other ref (they sort
    // first), so a graph-only default hides no later code default.
    let mut default = None;
    let mut graph = None;
    for r in refs {
        // `--full-tree`: the paths are from the repository's top, whichever
        // folder git runs in. `--literal-pathspecs`: a folder name is a
        // name, so `:api` or `*` is no pathspec magic. Only a file entry
        // is a config.
        let mut args = vec!["--literal-pathspecs", "ls-tree", "--full-tree", r, "--"];
        args.extend(&paths);
        let Some(entries) = git(&args) else {
            return Fetched::Unknown;
        };
        if entries
            .split(|b| *b == b'\n')
            .any(|e| e.split(|b| *b == b' ').nth(1) == Some(b"blob"))
        {
            if !defaults.contains(&r) {
                return Fetched::Graph {
                    graph: graph.unwrap_or(r).to_owned(),
                    default,
                };
            }
            let top_config = entries.split(|b| *b == b'\n').any(|e| {
                e.split(|b| *b == b'\t').nth(1) == Some(CONFIG_PATH.as_bytes())
                    && e.split(|b| *b == b' ').nth(1) == Some(b"blob")
            });
            if code_default(r, top_config) {
                default.get_or_insert_with(|| r.to_owned());
            } else {
                graph.get_or_insert(r);
            }
        }
    }
    match (graph, default) {
        (Some(g), default) => Fetched::Graph {
            graph: g.to_owned(),
            default,
        },
        (None, Some(d)) => Fetched::Graph {
            graph: d.clone(),
            default: Some(d),
        },
        (None, None) => Fetched::None,
    }
}

impl Fetched {
    /// Both ways to get the graph, since a fetched graph doesn't say which
    /// mode the team uses: a checkout-mode team merges it into its code,
    /// so a branch that predates it switches to or merges the default
    /// branch; a ref-mode team keeps it on a ref, which install adopts.
    /// With no `graph` (git couldn't say), install's help.
    fn routes(graph: Option<&str>, default: Option<&str>) -> String {
        let branch = match default {
            Some(d) => {
                // `refs/remotes/<remote>/<branch>`: a remote HEAD's
                // remote has no `/`, so the rest is the branch.
                let short = d
                    .strip_prefix("refs/remotes/")
                    .and_then(|rest| rest.split_once('/'))
                    .map_or(d, |(_, b)| b);
                let d = shell_word(d);
                format!(
                    "merge {d} (`git merge {d}`), or switch to {} once it's up to date with {d}",
                    shell_word(short)
                )
            }
            None => "switch to or merge your team's default branch".to_owned(),
        };
        format!(
            "if your team merges the graph into its code (checkout mode), {branch}; if it keeps \
             the graph on a ref of its own (ref mode), {}",
            Fetched::adopt(graph)
        )
    }

    /// The ref named as found: the default branch when it holds the graph.
    fn found<'a>(graph: &'a str, default: Option<&'a str>) -> &'a str {
        default.unwrap_or(graph)
    }
}

/// The hint for a folder with no config: adopt the fetched graph if the
/// clone has one, else start a graph or look elsewhere.
pub(crate) fn missing_config_hint(root: &Path) -> String {
    match fetched_graph(root) {
        Fetched::None => {
            format!("run from the repository root, pass --root <path>, or run `{BIN} init`")
        }
        Fetched::Graph { graph, default } => format!(
            "this clone fetched a graph on {}, which it hasn't adopted (`{BIN} init` would start \
             a second graph): {}",
            shell_word(Fetched::found(&graph, default.as_deref())),
            Fetched::routes(Some(&graph), default.as_deref())
        ),
        Fetched::Unknown => format!(
            "run from the repository root or pass --root <path>; git couldn't list this clone's \
             fetched refs, so check `git for-each-ref refs/remotes` for a graph before `{BIN} \
             init --new-graph`: {}",
            Fetched::routes(None, None)
        ),
    }
}

/// Init starts no second graph beside a fetched one unless `--new-graph`
/// asks for it: the clone's first step is adopting the graph. When git
/// can't say, init refuses too.
fn refuse_fork(root: &Path, args: &InitArgs) -> Result<(), GraphError> {
    if args.new_graph {
        return Ok(());
    }
    let (message, hint) = match fetched_graph(root) {
        Fetched::None => return Ok(()),
        Fetched::Graph { graph, default } => (
            format!(
                "{} holds a graph of this repository, and init would start a second one",
                shell_word(Fetched::found(&graph, default.as_deref()))
            ),
            format!(
                "{}; or `{BIN} init --new-graph` to start another graph anyway",
                Fetched::routes(Some(&graph), default.as_deref())
            ),
        ),
        Fetched::Unknown => (
            "git couldn't list this clone's fetched refs, so init can't tell whether it would \
             start a second graph"
                .to_owned(),
            format!(
                "check `git for-each-ref refs/remotes` for a graph: {}; or run `{BIN} init \
                 --new-graph` to start one anyway",
                Fetched::routes(None, None)
            ),
        ),
    };
    Err(GraphError::usage(Code::InitGraphFetched, message).with_hint(hint))
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
    let raw: Value = std::fs::read_to_string(root.join(CONFIG_PATH))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    existing_graph(root, args, cfg, &raw)
}

/// [`existing`] for a config already read (a checkout's, or the graph
/// ref's).
fn existing_graph(
    root: &Path,
    args: &InitArgs,
    cfg: config::Config,
    raw: &Value,
) -> Result<Report<InitData>, GraphError> {
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
        graph_ref: None,
        setup: None,
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
    // Project files are committed: the umask's mode, not 0600 (e2e S7).
    let mut f = at::create_public_file(&dir.fd, &tmp).map_err(|e| write_failed(rel, &e))?;
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

/// Whether `wt` is the clone's main checkout: its git folder is the
/// common one. A failed probe counts as linked, which only words the hint.
fn main_checkout(wt: &Path) -> bool {
    let dir = |arg: &str| {
        crate::coord::graph_git::git::command(wt)
            .args(["rev-parse", "--path-format=absolute", arg])
            .run(None)
            .ok()
            .filter(|o| o.status.success())
            // Only the line end: a folder name may end in a space (Terra
            // #20758).
            .and_then(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                std::fs::canonicalize(out.strip_suffix('\n').unwrap_or(&out)).ok()
            })
    };
    matches!((dir("--git-dir"), dir("--git-common-dir")), (Some(a), Some(b)) if a == b)
}

/// The graph configs in one checkout: tracked, untracked, ignored (an
/// ignored `sub/.mdkg/`), and on HEAD with only its removal staged
/// (Terra #20453), by relative path, sorted. The flag says the root
/// config is in the index or on HEAD. Reads trees and the index, never a
/// blob, and a git failure is an error rather than an absent graph (Terra
/// #20691).
fn checkout_graphs(dir: &Path) -> Result<(Vec<String>, bool), GraphError> {
    use crate::coord::graph_git::graph_error;
    const NESTED: &str = ":(glob)**/.mdkg/config.json";
    let listings: [&[&str]; 4] = [
        &["ls-files", "-z", "-c", "--", NESTED],
        &["ls-files", "-z", "-o", "--exclude-standard", "--", NESTED],
        &[
            "ls-files",
            "-z",
            "-o",
            "-i",
            "--exclude-standard",
            "--",
            NESTED,
        ],
        // The index and HEAD together (ls-tree takes no glob pathspec).
        &["ls-files", "-z", "-c", "--with-tree=HEAD", "--", NESTED],
    ];
    let mut found: Vec<String> = Vec::new();
    // The root config in the index (listings 0 and 3), or only on disk as
    // something has_config can't read (a dangling link; listings 1, 2).
    let mut root_indexed = false;
    for (i, listing) in listings.iter().enumerate() {
        let out = crate::coord::graph_git::git::command(dir)
            .args(*listing)
            .run(None)
            .map_err(|e| graph_error("io.git_failed", e.to_string()))?;
        if !out.status.success() {
            // An unborn branch has no HEAD to list.
            let unborn = i == 3
                && crate::coord::graph_git::git::command(dir)
                    .args(["rev-parse", "-q", "--verify", "HEAD^{commit}"])
                    .run(None)
                    .is_ok_and(|o| o.status.code() == Some(1));
            if unborn {
                continue;
            }
            let why = String::from_utf8_lossy(&out.stderr);
            let why = why.lines().next().unwrap_or("").trim();
            return Err(graph_error(
                "io.git_failed",
                format!(
                    "can't list the graphs in {} (git ls-files: {})",
                    dir.display(),
                    if why.is_empty() { "no message" } else { why }
                ),
            ));
        }
        // Only config files: the ignored listing also names ignored
        // embedded repositories (a linked worktree under an ignored
        // `.worktrees/`) as `dir/` entries, whatever they hold (Terra
        // #20565). The root config counts too: one only in the index
        // (staged, then removed from disk) is on neither disk nor HEAD
        // (Opus #20627).
        let listed = String::from_utf8_lossy(&out.stdout);
        let listed: Vec<&str> = listed.split('\0').collect();
        root_indexed |= (i == 0 || i == 3) && listed.contains(&CONFIG_PATH);
        found.extend(
            listed
                .into_iter()
                .filter(|p| *p == CONFIG_PATH || p.ends_with("/.mdkg/config.json"))
                .map(str::to_owned),
        );
    }
    found.sort();
    found.dedup();
    Ok((found, root_indexed))
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

    /// The dry run's next command keeps every option the run needs, and a
    /// failed restore is named in the hint (Terra #20413).
    #[test]
    fn init_setup_reruns_with_its_options_and_names_a_failed_restore() {
        let args = parsed_init(&[
            "--setup",
            "professional",
            "--graph-remote",
            "mine",
            "--work",
            "shared",
            "--graph-only",
            "--dry-run",
        ]);
        assert_eq!(
            rerun(&args, "professional"),
            format!(
                "{BIN} init --setup professional --graph-remote mine --work shared --graph-only"
            )
        );
        let e = GraphError::failure(Code::IoGitFailed, "boom").with_hint("first");
        let e = restored(e, vec!["git config --unset-all mdkg.graph.ref".into()]);
        let hint = e.diagnostic.hint.expect("hint");
        assert!(hint.starts_with("first. "), "{hint}");
        assert!(
            hint.contains("`git config --unset-all mdkg.graph.ref`"),
            "{hint}"
        );
        let e = restored(GraphError::failure(Code::IoGitFailed, "boom"), Vec::new());
        assert!(e.diagnostic.hint.is_none());
    }

    /// A main checkout whose `.git` is a file (--separate-git-dir, a git
    /// folder name ending in a space) is still the main one; a failed
    /// listing names git's reason (Opus #20759, Terra #20758).
    #[test]
    fn main_checkout_by_git_dir_and_listing_errors_say_why() {
        let base = std::env::temp_dir().join(format!(
            "mdkg-init-main-checkout-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("base");
        let main = base.join("main");
        let ok = std::process::Command::new("git")
            .args(["init", "-q", "--separate-git-dir"])
            .arg(base.join("git dir "))
            .arg(&main)
            .status()
            .expect("git runs");
        assert!(ok.success());
        assert!(main.join(".git").is_file());
        assert!(main_checkout(&main));
        assert!(!main_checkout(&base.join("absent")));
        // A damaged index fails the listing itself.
        let damaged = base.join("damaged");
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&damaged)
            .status()
            .expect("git runs");
        assert!(ok.success());
        std::fs::write(damaged.join(".git/index"), b"not an index").expect("index");
        let e = checkout_graphs(&damaged).expect_err("damaged index");
        let message = &e.diagnostic.message;
        assert!(message.contains("(git ls-files: "), "{message}");
        assert!(!message.contains("no message"), "{message}");
        std::fs::remove_dir_all(&base).expect("clean up");
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
    fn adoption_hints_quote_what_a_shell_would_read() {
        assert_eq!(
            shell_word("refs/heads/mdkg-graph:.mdkg/README.md"),
            "refs/heads/mdkg-graph:.mdkg/README.md"
        );
        assert_eq!(shell_word("refs/heads/$(id):x"), "'refs/heads/$(id):x'");
        assert_eq!(shell_word("it's"), "'it'\"'\"'s'");
        assert_eq!(
            Fetched::adopt(Some(PERSONAL_REF)),
            format!("run `{BIN} install --setup personal`")
        );
        assert!(!Fetched::adopt(Some("refs/remotes/g$(id)/mdkg-graph")).contains("$(id)"));
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
            in_checkout: false,
            new_graph: false,
            setup: None,
            graph_remote: None,
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
