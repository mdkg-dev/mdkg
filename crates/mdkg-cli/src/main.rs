//! `mdkg`: the Rust kernel's command-line interface.
//!
//! Every command follows the output contract: `--json` writes one
//! envelope with coded diagnostics, and the exit code is non-zero exactly
//! when the envelope's `ok` is false.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::config;

/// This binary's name as invoked (argv[0]), for hints that name a command
/// to run: `mdkg`, or the name a build is installed under beside another.
use mdkg_format::program::BIN;

/// A help page's examples, named with this binary.
macro_rules! examples {
    ($($line:literal),+ $(,)?) => {
        concat!("Examples:", $("\n  ", env!("CARGO_BIN_NAME"), " ", $line),+)
    };
}

mod affected;
mod agents;
mod anchor;
mod auth;
#[cfg(unix)]
mod authority;
#[cfg(not(unix))]
#[path = "authority_none.rs"]
mod authority;
mod backup;
mod compat;
mod config_cmd;
mod coord;
mod derived;
mod doctor;
mod export;
mod graph;
mod graph_repair;
mod hook;
mod init;
mod mcp;
mod migrate;
mod msg;
mod output;
mod pack;
mod read;
mod retype;
mod run;
mod schema;
mod scratch;
mod search;
mod skills;
mod verify;
mod version;
mod work;
mod write;

#[derive(Debug, Parser)]
#[command(
    name = "mdkg",
    version = version::LINE,
    about = "Local-first project memory for humans and agents"
)]
struct Cli {
    /// Run against this repository root (default: the current directory).
    #[arg(long, short = 'r', global = true, help_heading = "Global options")]
    root: Option<PathBuf>,
    /// Act as this actor in coordination commands (default: $MDKG_ACTOR,
    /// then the worktree's folder name).
    #[arg(
        long = "as",
        global = true,
        value_name = "ACTOR",
        help_heading = "Global options"
    )]
    actor: Option<String>,
    /// Let a development build upgrade the coordinator store.
    ///
    /// The store is copied first; never use this on a live repository's
    /// store. Installed builds always may; only a build running from a Cargo
    /// build folder needs this.
    #[arg(
        long = "allow-store-upgrade",
        global = true,
        help_heading = "Global options"
    )]
    allow_store_upgrade: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TagsModeArg {
    Any,
    All,
}

#[derive(Debug, Args)]
struct FilterArgs {
    /// Workspace alias (`all` means every workspace).
    #[arg(long)]
    ws: Option<String>,
    /// Comma-separated node types; nodes of any of them match.
    #[arg(long = "type", value_delimiter = ',')]
    node_type: Vec<String>,
    /// Comma-separated statuses; nodes with any of them match.
    #[arg(long, value_delimiter = ',')]
    status: Vec<String>,
    /// Comma-separated tags.
    #[arg(long, value_delimiter = ',')]
    tags: Vec<String>,
    /// Whether nodes need any or all of the tags.
    #[arg(long, value_enum)]
    tags_mode: Option<TagsModeArg>,
    /// Comma-separated node fields, or `all` (default: id, type, title,
    /// status, priority, path).
    #[arg(long)]
    fields: Option<String>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

/// Options for `new` (and `checkpoint new`).
#[derive(Debug, Args)]
struct NewArgs {
    /// Node type: task, bug, feat, spike, test, epic, goal, checkpoint, edd,
    /// dec, prd, prop or rule.
    #[arg(value_name = "TYPE")]
    node_type: String,
    #[command(flatten)]
    rest: NewRest,
}

#[derive(Debug, Args)]
struct NewRest {
    /// Title; unquoted words are joined.
    #[arg(required = true, num_args = 1..)]
    title: Vec<String>,
    /// A lasting status: backlog, todo or done (decisions: proposed and so on).
    #[arg(long)]
    status: Option<String>,
    /// Work items: 0 (highest) to 9 (lowest, the default), unless the
    /// config sets another range.
    #[arg(long)]
    priority: Option<i64>,
    /// The parent node.
    #[arg(long)]
    parent: Option<String>,
    /// The epic it belongs to.
    #[arg(long)]
    epic: Option<String>,
    /// The node before it in a chain.
    #[arg(long)]
    prev: Option<String>,
    /// The node after it in a chain.
    #[arg(long)]
    next: Option<String>,
    /// Comma-separated tags.
    #[arg(long, value_delimiter = ',')]
    tags: Vec<String>,
    /// Comma-separated owners.
    #[arg(long, value_delimiter = ',')]
    owners: Vec<String>,
    /// Comma-separated nodes it refers to.
    #[arg(long, value_delimiter = ',')]
    refs: Vec<String>,
    /// Comma-separated nodes it relates to.
    #[arg(long, value_delimiter = ',')]
    relates: Vec<String>,
    /// Comma-separated nodes that block it.
    #[arg(long = "blocked-by", value_delimiter = ',')]
    blocked_by: Vec<String>,
    /// Comma-separated nodes it blocks.
    #[arg(long, value_delimiter = ',')]
    blocks: Vec<String>,
    /// Comma-separated skill slugs.
    #[arg(long, value_delimiter = ',')]
    skills: Vec<String>,
    /// Checkpoints: the nodes covered.
    #[arg(long, value_delimiter = ',')]
    scope: Vec<String>,
    /// Checkpoints: implementation, test-proof, goal-closeout, audit or handoff.
    #[arg(long)]
    kind: Option<String>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

impl NewRest {
    fn spec(self, node_type: &str) -> write::NewSpec {
        write::NewSpec {
            node_type: if node_type == "chk" {
                "checkpoint".into()
            } else {
                node_type.to_lowercase()
            },
            title: self.title.join(" ").trim().to_string(),
            status: self.status,
            priority: self.priority,
            parent: self.parent,
            epic: self.epic,
            prev: self.prev,
            next: self.next,
            tags: self.tags,
            owners: self.owners,
            refs: self.refs,
            relates: self.relates,
            blocked_by: self.blocked_by,
            blocks: self.blocks,
            skills: self.skills,
            scope: self.scope,
            kind: self.kind,
        }
    }
}

#[derive(Debug, Subcommand)]
enum GoalCommand {
    /// Show the active goal and any leases inside it.
    #[command(after_help = examples!("goal current"))]
    Current {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the next work item in a goal (default: the active goal).
    #[command(after_help = examples!("goal next", "goal next <goal-id> --json"))]
    Next {
        /// The goal's id.
        goal: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Make a goal the active one, pausing any other.
    #[command(after_help = examples!("goal activate <goal-id>"))]
    Activate {
        /// The goal's id.
        goal: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Pause a goal.
    #[command(after_help = examples!("goal pause <goal-id>"))]
    Pause {
        /// The goal's id.
        goal: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Mark a goal achieved.
    #[command(after_help = examples!("goal done <goal-id>"))]
    Done {
        /// The goal's id.
        goal: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// 0.6.0 `goal claim [goal] [work]`: claims the work item, or the
    /// goal's next item (now `goal next`, then `claim`).
    #[command(hide = true)]
    Claim {
        #[arg(num_args = 0..=2)]
        args: Vec<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List nodes.
    #[command(after_help = examples!("list --type task --status todo", "list --blocked --json"))]
    List {
        #[command(flatten)]
        filters: FilterArgs,
        /// Only nodes in this epic (an id, id prefix or title fragment).
        #[arg(long)]
        epic: Option<String>,
        /// Only nodes with this priority.
        #[arg(long)]
        priority: Option<i64>,
        /// Only blocked nodes.
        #[arg(long)]
        blocked: bool,
    },
    /// Show one node.
    #[command(after_help = examples!("show <id>", "show <id> --meta --json"))]
    Show {
        /// The node: a full id, a unique id prefix or a title fragment.
        id: String,
        /// Workspace hint.
        #[arg(long)]
        ws: Option<String>,
        /// Omit the body.
        #[arg(long)]
        meta: bool,
        /// Comma-separated node fields, or `all` (the default).
        #[arg(long)]
        fields: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Search ids, titles, fields and bodies, best match first.
    #[command(after_help = examples!("search lease renew", "search lease renew --limit 10 --json"))]
    Search(Box<search::SearchArgs>),
    /// Rebuild the derived index, or show its state.
    #[command(subcommand, after_help = examples!("index status", "index rebuild"))]
    Index(derived::IndexCommand),
    /// Query the graph: refs, backrefs, the ready set, a goal's critical path.
    #[command(
        subcommand,
        after_help = examples!("graph ready --limit 5", "graph backrefs <id>", "graph critical-path <goal-id>")
    )]
    Graph(graph::GraphCommand),
    /// Locate local scratch shared by the repository's worktrees.
    #[command(subcommand, after_help = examples!("scratch path", "scratch path --shared"))]
    Scratch(scratch::ScratchCommand),
    /// Change where work definitions live.
    #[command(subcommand)]
    Work(work::WorkCommand),
    /// Inspect configuration layers and their sources.
    #[command(subcommand)]
    Config(config_cmd::ConfigCommand),
    /// Build a markdown context pack around a node or a query, in a token
    /// budget.
    #[command(after_help = examples!("pack <id>"))]
    Pack(Box<pack::PackArgs>),
    /// Validate the graph.
    #[command(after_help = examples!("validate", "validate --changed-only --json"))]
    Validate {
        /// Print JSON.
        #[arg(long)]
        json: bool,
        /// Don't print warnings.
        #[arg(long)]
        quiet: bool,
        /// Print every warning (default: one summary line per warning code
        /// that occurs more than once).
        #[arg(long, conflicts_with = "quiet")]
        warnings: bool,
        /// Report only on files changed since the branch left main.
        #[arg(long)]
        changed_only: bool,
    },
    /// Create a node.
    ///
    /// In a 0.7 graph its id is minted (`<type>-<uuidv7>`); in a 0.6.0 graph
    /// the shared coordinator numbers it.
    #[command(after_help = examples!(
        "new task Fix the login timeout --status todo",
        "new task \"Add retries\" --parent <goal-id> --priority 2 --json"
    ))]
    New(NewArgs),
    /// Finish a work item: record its version and write `status: done`.
    #[command(after_help = examples!("done <id>", "done <id> --checkpoint"))]
    Done {
        /// Node id.
        target: String,
        /// Also create a checkpoint node covering it.
        #[arg(long)]
        checkpoint: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Set a temporary status (review or blocked), or clear it.
    #[command(after_help = examples!("update <id> --status review", "update <id> --clear"))]
    Update {
        /// Node id; with --clear, also the path `status` lists for a state
        /// whose node isn't in this worktree (yours only).
        target: String,
        /// review or blocked.
        #[arg(long)]
        status: Option<String>,
        /// Clear the temporary status.
        #[arg(long, conflicts_with = "status")]
        clear: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the next work item to pick up (the full record).
    #[command(after_help = examples!("next", "next --json"))]
    Next {
        /// Follow this node's `next` edge instead.
        id: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show and change goals: current, next, activate, pause, done.
    #[command(subcommand, after_help = examples!("goal current", "goal activate <goal-id>"))]
    Goal(GoalCommand),
    /// Report files whose frontmatter isn't canonical; `--write` fixes them.
    #[command(after_help = examples!("format", "format --write"))]
    Format {
        /// Node ids (default: every node).
        ids: Vec<String>,
        /// Rewrite the files (skips files leased by others).
        #[arg(long)]
        write: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Change a node's type: its id and every ref to it (format 0.7).
    #[command(after_help = examples!("retype <id> bug"))]
    Retype {
        /// The node: an id, legacy id or handle.
        target: String,
        /// The new type.
        new_type: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    #[command(subcommand, hide = true)]
    Task(compat::TaskCommand),
    #[command(subcommand, hide = true)]
    Checkpoint(compat::CheckpointCommand),
    /// Claim a node, file, folder, role or turn.
    ///
    /// A role is `role:<name>` (its mailbox), a turn `run:<name>`. Claiming
    /// again renews your lease, except a turn, which `renew` extends.
    #[command(after_help = examples!("claim <id>", "claim --new src/new_module.rs", "claim role:reviewer"))]
    Claim {
        /// Node id, file or folder path, `role:<name>` or `run:<name>`
        /// (names: lowercase a-z, 0-9, `.`, `_`, `-` and `:`, up to 120).
        target: String,
        /// A path you're about to create: lease it as a path, without the
        /// missing-file warning.
        ///
        /// A path no worktree has yet is leased as a folder would be; your
        /// lease keeps that mode until you release it.
        #[arg(long)]
        new: bool,
        /// Keep a path the config shares to yourself for this claim.
        #[arg(long)]
        exclusive: bool,
        /// For a lease authority's host: the target is a canonical scope
        /// (relative to the repository's top), at least this strict.
        #[arg(long, hide = true, value_parser = ["exclusive", "shared"], conflicts_with = "new")]
        mode: Option<String>,
        /// With --mode: the scope is this node's (always exclusive).
        #[arg(long, hide = true, requires = "mode")]
        node: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check whether you hold a lease covering a scope.
    ///
    /// The scope is relative to the repository root. Read-only, as a lease
    /// authority asks it.
    #[command(after_help = examples!("holds src/lib.rs"))]
    Holds {
        /// The scope.
        scope: String,
        /// The node the scope is (its qualified id): held only if nobody
        /// holds the node under another path.
        #[arg(long)]
        node: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Renew every active lease you hold.
    #[command(after_help = examples!("renew"))]
    Renew {
        /// Internal receiver heartbeat: only this local turn, no authority.
        #[arg(long, hide = true)]
        local_turn: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Give up your lease without finishing; a lease that already ended is
    /// fine.
    ///
    /// `--all` releases every active lease you hold: run it once your head
    /// has merged and you've committed everything. Each lease is released
    /// on its own, so running it again finishes a run that stopped part
    /// way.
    #[command(after_help = examples!(
        "release <id>",
        "release src/lib.rs",
        "release --all"
    ))]
    Release {
        /// Node id, file or folder path, `role:<name>`, `run:<name>`, or
        /// lease id.
        #[arg(required_unless_present_any = ["scope", "all"])]
        target: Option<String>,
        /// For a lease authority's host: release your lease on this
        /// canonical scope (relative to the repository's top) instead.
        #[arg(long, hide = true, conflicts_with = "target")]
        scope: Option<String>,
        /// Release every active lease you hold.
        #[arg(long, conflicts_with_all = ["target", "scope"])]
        all: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show leases, the settlement queue and your unacked mail.
    ///
    /// Leases are yours when the actor is named (--as or $MDKG_ACTOR),
    /// everyone's with --all; JSON always lists everyone's, marking yours.
    #[command(after_help = examples!("status", "status --all"))]
    Status {
        /// Show everyone's leases and expired leases.
        #[arg(long)]
        all: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Stack this worktree on unmerged versions it lacks.
    #[command(after_help = examples!("sync", "sync <id>"))]
    Sync {
        /// Node id or folder path (default: the whole graph).
        target: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Drop a changeset whose branch won't merge, and work built on it.
    #[command(after_help = examples!("abandon cs-3", "abandon --branch my-branch"))]
    Abandon {
        /// Changeset id (`cs-<n>`).
        #[arg(required_unless_present = "branch")]
        changeset: Option<String>,
        /// Abandon every open changeset from this branch instead.
        #[arg(long, conflicts_with = "changeset")]
        branch: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Serve the graph and leases to coding agents over stdio MCP.
    #[command(after_help = examples!("mcp --as <agent>"))]
    Mcp,
    /// Send and read messages: send, inbox, wait, ack, thread, import.
    #[command(subcommand, after_help = examples!("msg inbox", "msg send --room dev --kind note --text \"Tests pass on main\""))]
    Msg(msg::MsgCommand),
    /// Manage rooms: create, join, leave, list.
    #[command(subcommand, after_help = examples!("room list", "room join dev"))]
    Room(msg::RoomCommand),
    /// Manage definitions and run agents: add, edit, rm, up, status, resume.
    #[command(subcommand, after_help = examples!("agents status", "agents up --once"))]
    Agents(agents::AgentsCommand),
    /// The API key fallback's helper: `auth key <provider>`.
    #[command(subcommand, after_help = examples!("auth key anthropic"))]
    Auth(auth::AuthCommand),
    /// What an agent harness's hook runs: mail at turn start, a reminder
    /// before stopping with unread mail, an idle wake, and the edit guard.
    ///
    /// Reads the harness's hook input on stdin and prints its output. Says
    /// nothing outside a graph; never shows a message's body.
    #[command(after_help = examples!("hook start --harness claude", "hook edit --harness opencode"))]
    Hook(hook::HookArgs),
    #[command(name = "__receiver-queue-guard", hide = true)]
    ReceiverQueueGuard { spec: String, owner: u32 },
    /// Set up Claude Code, Codex and OpenCode for this project.
    ///
    /// Writes exactly the projections .mdkg/config.json lists: MCP configs,
    /// the AGENTS.md block, skill mirrors and hooks. --enable and --disable
    /// change the list, then sync.
    #[command(after_help = examples!(
        "install --enable agents-md,mcp:claude,skills:.claude/skills --dry-run",
        "install",
        "install --disable mcp:opencode"
    ))]
    Install {
        /// Show what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Put back everything install changed here, exactly as it was;
        /// refuses where a file was edited since.
        #[arg(long)]
        undo: bool,
        /// The mdkg binary the MCP configs run (default: this one, by its
        /// bare name when `PATH` finds this same file, else by its path as
        /// invoked).
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Projections to add to the config's list, comma-separated:
        /// agents-md, mcp:claude, mcp:codex, mcp:opencode,
        /// skills:.agents/skills, skills:.claude/skills, hooks:claude,
        /// hooks:codex, hooks:opencode.
        #[arg(long, value_name = "LIST", conflicts_with = "undo")]
        enable: Option<String>,
        /// Projections to take off the list, comma-separated; mdkg's part
        /// of their files is removed, the rest kept.
        #[arg(long, value_name = "LIST", conflicts_with = "undo")]
        disable: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// List or restore copies of the coordinator store.
    #[command(subcommand, after_help = examples!("store snapshots"))]
    Store(backup::StoreCommand),
    /// Write the record chain's head hash to an anchor file.
    ///
    /// The file is .mdkg/anchors/<store-id>.json, for you to commit.
    #[command(after_help = examples!("anchor"))]
    Anchor {
        /// Replace an anchor the chain no longer holds (after a restore).
        #[arg(long)]
        force: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Export work state as a bundle anyone can verify.
    ///
    /// `--work` writes the coordinator's record chain.
    #[command(after_help = examples!("export --work", "export --work --out work.jsonl"))]
    Export {
        /// The coordinator's record chain.
        #[arg(long)]
        work: bool,
        /// Where to write it (default: mdkg-work-<store>-<seq>.jsonl here).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Replace the file if it exists.
        #[arg(long)]
        force: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Verify a work bundle, or this repository's store, against an anchor.
    ///
    /// Verifying a bundle needs no repository.
    #[command(after_help = examples!("verify work.jsonl", "verify"))]
    Verify {
        /// The bundle; without one, the repository's store.
        bundle: Option<PathBuf>,
        /// The anchor to check against (default for the store: the
        /// graph's own).
        #[arg(long)]
        anchor: Option<PathBuf>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Bring a 0.6.0 graph onto 0.7.0.
    ///
    /// Plan with --dry-run, then --apply <hash>; --status shows how far it
    /// got, and --rollback reverts it.
    #[command(after_help = examples!("migrate --dry-run", "migrate --apply <hash>", "migrate --status"))]
    Migrate(migrate::MigrateArgs),
    /// Show which build this is: version, commit, dirty tree, build date.
    #[command(after_help = examples!("version --json"))]
    Version {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start a graph here: config, setup README and the mdkg-workflow skill.
    ///
    /// Writes only inside .mdkg. Integrations are opt-in with `install
    /// --enable`; .mdkg/README.md explains the setup steps.
    #[command(after_help = examples!("init", "init --dry-run"))]
    Init(init::InitArgs),
    /// Check this setup: build, PATH, git, config, graph, store, mirrors,
    /// MCP, agents, hooks.
    ///
    /// Changes nothing.
    #[command(after_help = examples!("doctor", "doctor --json"))]
    Doctor(doctor::DoctorArgs),
    /// List, show, validate, add and sync skills in .mdkg/skills.
    #[command(subcommand, alias = "skill", after_help = examples!("skills list", "skills show <slug>"))]
    Skills(skills::SkillsCommand),
    /// The tests a change needs: a selective run, or a full one when in doubt.
    #[command(after_help = examples!("affected --base main --json"))]
    Affected {
        /// The commit the change starts from (for a PR, its base).
        #[arg(long)]
        base: String,
        /// The commit the change ends at; any commit (the checkout isn't read).
        #[arg(long, default_value = "HEAD")]
        head: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the JSON Schema of a command's output.
    #[command(after_help = examples!("schema", "schema next"))]
    Schema {
        /// Command name (default: list them).
        command: Option<String>,
        /// Write every schema into this folder instead.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

impl Command {
    /// Commands that can mutate graph files, including compatibility routes.
    /// Keep this before config parsing: checkout damage may include config.json.
    fn writes_graph(&self) -> bool {
        matches!(
            self,
            Self::New(_)
                | Self::Done { .. }
                | Self::Update { .. }
                | Self::Checkpoint(_)
                | Self::Task(compat::TaskCommand::Update { .. } | compat::TaskCommand::Done { .. })
                | Self::Goal(
                    GoalCommand::Done { .. }
                        | GoalCommand::Activate { .. }
                        | GoalCommand::Pause { .. }
                )
                | Self::Format { write: true, .. }
                | Self::Retype { .. }
                | Self::Sync { .. }
                | Self::Install { .. }
                | Self::Init(_)
                | Self::Migrate(_)
                | Self::Work(work::WorkCommand::Localize(_))
                | Self::Anchor { .. }
                | Self::Agents(
                    agents::AgentsCommand::Add(_)
                        | agents::AgentsCommand::Edit(_)
                        | agents::AgentsCommand::Rm { .. }
                        | agents::AgentsCommand::Up(_)
                )
                | Self::Skills(
                    skills::SkillsCommand::Add { .. } | skills::SkillsCommand::Sync { .. }
                )
        )
    }

    fn name(&self) -> &'static str {
        match self {
            Command::List { .. } => "list",
            Command::Show { .. } => "show",
            Command::Search(_) => "search",
            Command::Index(c) => c.name(),
            Command::Graph(c) => c.name(),
            Command::Scratch(c) => c.name(),
            Command::Work(c) => c.name(),
            Command::Config(c) => c.name(),
            Command::Pack(_) => "pack",
            Command::Validate { .. } => "validate",
            Command::Claim { .. } => "claim",
            Command::Release { .. } => "release",
            Command::Holds { .. } => "holds",
            Command::Renew { .. } => "renew",
            Command::Status { .. } => "status",
            Command::Sync { .. } => "sync",
            Command::Abandon { .. } => "abandon",
            Command::Version { .. } => "version",
            Command::Affected { .. } => "affected",
            Command::Schema { .. } => "schema",
            Command::Mcp => "mcp",
            Command::New(_) | Command::Checkpoint(_) => "new",
            Command::Done { .. } => "done",
            Command::Update { .. } => "update",
            Command::Task(t) => t.name(),
            Command::Next { .. } => "next",
            Command::Goal(g) => match g {
                GoalCommand::Current { .. } => "goal.current",
                GoalCommand::Next { .. } => "goal.next",
                GoalCommand::Activate { .. } => "goal.activate",
                GoalCommand::Pause { .. } => "goal.pause",
                GoalCommand::Done { .. } => "goal.done",
                GoalCommand::Claim { args, .. } => compat::goal_claim_name(args),
            },
            Command::Format { .. } => "format",
            Command::Retype { .. } => "retype",
            Command::Msg(m) => m.name(),
            Command::Room(r) => r.name(),
            Command::Agents(a) => a.name(),
            Command::Auth(a) => a.name(),
            Command::Hook(_) => "hook",
            Command::ReceiverQueueGuard { .. } => "cli",
            Command::Install { .. } => "install",
            Command::Store(s) => s.name(),
            Command::Anchor { .. } => "anchor",
            Command::Export { .. } => "export",
            Command::Verify { .. } => "verify",
            Command::Init(_) => "init",
            Command::Doctor(_) => "doctor",
            Command::Skills(s) => s.name(),
            Command::Migrate(_) => "migrate",
        }
    }

    fn json(&self) -> bool {
        match self {
            Command::List { filters, .. } => filters.json,
            Command::Search(a) => a.filters.json,
            Command::Index(c) => c.json(),
            Command::Graph(c) => c.json(),
            Command::Scratch(c) => c.json(),
            Command::Work(c) => c.json(),
            Command::Config(c) => c.json(),
            Command::Pack(a) => a.json,
            Command::Show { json, .. }
            | Command::Validate { json, .. }
            | Command::Claim { json, .. }
            | Command::Release { json, .. }
            | Command::Holds { json, .. }
            | Command::Renew { json, .. }
            | Command::Status { json, .. }
            | Command::Sync { json, .. }
            | Command::Abandon { json, .. }
            | Command::Done { json, .. }
            | Command::Update { json, .. }
            | Command::Next { json, .. }
            | Command::Affected { json, .. }
            | Command::Format { json, .. }
            | Command::Retype { json, .. } => *json,
            Command::New(a) => a.rest.json,
            Command::Checkpoint(c) => c.json(),
            Command::Task(t) => t.json(),
            Command::Goal(g) => match g {
                GoalCommand::Current { json }
                | GoalCommand::Next { json, .. }
                | GoalCommand::Activate { json, .. }
                | GoalCommand::Pause { json, .. }
                | GoalCommand::Done { json, .. }
                | GoalCommand::Claim { json, .. } => *json,
            },
            Command::Msg(m) => m.json(),
            Command::Room(r) => r.json(),
            Command::Agents(a) => a.json(),
            Command::Auth(_) => false,
            Command::Hook(_) => false,
            Command::ReceiverQueueGuard { .. } => false,
            Command::Install { json, .. } => *json,
            Command::Store(s) => s.json(),
            Command::Anchor { json, .. }
            | Command::Export { json, .. }
            | Command::Verify { json, .. } => *json,
            Command::Init(a) => a.json,
            Command::Doctor(a) => a.json,
            Command::Skills(s) => s.json(),
            Command::Migrate(a) => a.json,
            Command::Version { json } => *json,
            Command::Schema { .. } => true,
            Command::Mcp => false,
        }
    }
}

/// Reports an error that happened before a command could run.
fn early_failure(command: &str, json: bool, d: Diagnostic, exit: u8) -> ExitCode {
    // `agents up --json` prints one envelope per line, its failures too.
    if command == "agents.up" && json {
        agents::print_line_failure(d);
        return ExitCode::from(exit);
    }
    let report: output::Report<()> = output::Report {
        data: None,
        diagnostics: vec![d],
        failure: Some(mdkg_graph::ErrorKind::Usage),
    };
    output::emit(command, json, report, |_| {});
    ExitCode::from(exit)
}

fn run(cli: Cli, root: &Path) -> u8 {
    let actor = cli.actor;
    match cli.command {
        Command::List {
            filters,
            epic,
            priority,
            blocked,
        } => read::list(root, &filters, epic, priority, blocked),
        Command::Show {
            id,
            ws,
            meta,
            fields,
            json,
        } => read::show(root, &id, ws.as_deref(), meta, fields.as_deref(), json),
        Command::Search(a) => search::run(root, &a),
        Command::Index(c) => derived::run(root, &c),
        Command::Graph(c) => graph::run(root, &c),
        Command::Pack(a) => pack::run(root, &a),
        Command::Validate {
            json,
            quiet,
            warnings,
            changed_only,
        } => {
            let shown = if quiet {
                read::Warnings::None
            } else if warnings {
                read::Warnings::All
            } else {
                read::Warnings::Summary
            };
            read::validate(root, json, shown, changed_only)
        }
        Command::New(args) => {
            let json = args.rest.json;
            let spec = args.rest.spec(&args.node_type);
            write::new_node(root, actor, spec, json)
        }
        Command::Checkpoint(c) => compat::checkpoint(root, actor, c),
        Command::Done {
            target,
            checkpoint,
            json,
        } => write::done(root, actor, &target, checkpoint, json),
        Command::Update {
            target,
            status,
            clear,
            json,
        } => write::update(root, actor, &target, status.as_deref(), clear, json),
        Command::Next { id, json } => write::next(root, actor, id.as_deref(), json),
        Command::Goal(g) => {
            let (action, json) = match g {
                GoalCommand::Current { json } => (write::GoalAction::Current, json),
                GoalCommand::Next { goal, json } => (write::GoalAction::Next(goal), json),
                GoalCommand::Activate { goal, json } => (write::GoalAction::Activate(goal), json),
                GoalCommand::Pause { goal, json } => (write::GoalAction::Pause(goal), json),
                GoalCommand::Done { goal, json } => (write::GoalAction::Done(goal), json),
                GoalCommand::Claim { args, json } => {
                    return compat::goal_claim(root, actor, &args, json);
                }
            };
            write::goal(root, actor, action, json)
        }
        Command::Format {
            ids,
            write: w,
            json,
        } => write::format(root, actor, &ids, w, json),
        Command::Retype {
            target,
            new_type,
            json,
        } => retype::retype(root, actor, &target, &new_type, json),
        Command::Task(t) => compat::task(root, actor, t),
        Command::Claim {
            target,
            new,
            exclusive,
            mode,
            node,
            json,
        } => coord::claim(
            root,
            actor,
            &target,
            coord::ClaimOptions {
                new,
                exclusive,
                mode: mode.map(|m| {
                    if m == "shared" {
                        mdkg_coord::Mode::Shared
                    } else {
                        mdkg_coord::Mode::Exclusive
                    }
                }),
                node,
                exact: false,
            },
            json,
        ),
        Command::Holds { scope, node, json } => {
            coord::holds(root, actor, &scope, node.as_deref(), json)
        }
        Command::Renew { json, local_turn } => {
            coord::renew(root, actor, json, local_turn.as_deref())
        }
        Command::Release {
            all: true, json, ..
        } => coord::release_all(root, actor, json),
        Command::Release {
            target,
            scope,
            json,
            ..
        } => {
            let which = match (&target, &scope) {
                (_, Some(s)) => coord::ReleaseTarget::Scope(s),
                (Some(t), None) => coord::ReleaseTarget::Named(t),
                (None, None) => coord::ReleaseTarget::Named(""),
            };
            coord::release(root, actor, which, json)
        }
        Command::Status { all, json } => coord::status(root, actor, all, json),
        Command::Sync { target, json } => coord::sync(root, actor, target.as_deref(), json),
        Command::Abandon {
            changeset,
            branch,
            json,
        } => coord::abandon(root, changeset.as_deref(), branch.as_deref(), json),
        Command::Mcp => mcp::serve(root, actor),
        Command::Msg(m) => msg::run(root, actor, &m),
        Command::Room(r) => msg::room(root, actor, &r),
        Command::Agents(a) => agents::run(root, actor, &a),
        Command::Scratch(s) => scratch::run(root, actor, &s),
        Command::Work(w) => work::run(root, actor, &w),
        Command::Config(_) => unreachable!("config inspection runs without the coordinator"),
        Command::Auth(_) => unreachable!("auth runs without a graph"),
        Command::Hook(_) => unreachable!("hooks find their own graph"),
        Command::ReceiverQueueGuard { .. } => unreachable!("guard runs before graph discovery"),
        Command::Install {
            dry_run,
            undo,
            binary,
            enable,
            disable,
            json,
        } => run::install(
            root,
            actor,
            run::InstallChoice {
                dry_run,
                undo,
                binary: binary.as_deref(),
                enable: enable.as_deref(),
                disable: disable.as_deref(),
            },
            json,
        ),
        Command::Store(s) => backup::run(root, &s),
        Command::Anchor { force, json } => anchor::run(root, force, json),
        Command::Export {
            work,
            out,
            force,
            json,
        } => export::run(root, work, out.as_deref(), force, json),
        Command::Verify {
            bundle,
            anchor,
            json,
        } => match bundle {
            Some(b) => verify::bundle(&b, anchor.as_deref(), json),
            None => verify::store(root, anchor.as_deref(), json),
        },
        Command::Skills(s) => skills::run(root, &s),
        Command::Init(_) | Command::Doctor(_) => {
            unreachable!("init and doctor run without a graph")
        }
        Command::Migrate(a) => migrate::run(root, &a),
        Command::Affected { .. } => unreachable!("affected reads the committed config"),
        Command::Schema { .. } => unreachable!("schema runs without a graph"),
        Command::Version { .. } => unreachable!("version runs without a graph"),
    }
}

/// Whether `s` looks like a flag (`-x`, `--name-2`), not typed text that
/// happens to start with a dash.
fn flag_token(s: &str) -> bool {
    let name = s.strip_prefix("--").or_else(|| s.strip_prefix('-'));
    name.is_some_and(|n| {
        n.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && n.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// A parse error in words, naming the argument but never quoting a value
/// that was typed: free-text values may contain secrets.
fn usage_message(e: &clap::Error) -> String {
    use clap::error::{ContextKind, ContextValue, ErrorKind as Kind};
    let named = |kind| match e.get(kind) {
        Some(ContextValue::String(s)) => Some(s.clone()),
        Some(ContextValue::Strings(v)) => Some(v.join(", ")),
        _ => None,
    };
    let (what, arg) = match e.kind() {
        Kind::InvalidValue | Kind::ValueValidation => {
            let empty = matches!(
                e.get(ContextKind::InvalidValue),
                Some(ContextValue::String(v)) if v.is_empty()
            ) || e.get(ContextKind::InvalidValue).is_none();
            let what = if empty {
                "missing value for"
            } else {
                "invalid value for"
            };
            (what, named(ContextKind::InvalidArg))
        }
        // A typed flag is named, without anything after `=`; a stray
        // positional is typed text, so it isn't quoted.
        // What was typed is never echoed: a flag-looking word can be a
        // secret too. Clap's suggestion is a real flag, so it's named.
        Kind::UnknownArgument => match named(ContextKind::SuggestedArg) {
            Some(s) => ("unknown argument; did you mean", Some(s)),
            None if named(ContextKind::InvalidArg).is_some_and(|a| flag_token(&a)) => {
                ("unknown argument", None)
            }
            None => ("unexpected extra argument", None),
        },
        Kind::MissingRequiredArgument => {
            ("missing required argument", named(ContextKind::InvalidArg))
        }
        Kind::InvalidSubcommand => ("unknown command", None),
        Kind::MissingSubcommand | Kind::DisplayHelpOnMissingArgumentOrSubcommand => {
            ("missing command", None)
        }
        Kind::ArgumentConflict => {
            let arg = named(ContextKind::InvalidArg);
            let prior = named(ContextKind::PriorArg);
            if arg.is_some() && arg == prior {
                ("given more than once:", arg)
            } else {
                // Clap supplies declared argument names here, including
                // value placeholders, never the values that were typed.
                let args = match (arg, prior) {
                    (Some(a), Some(b)) => Some(format!("{a} with {b}")),
                    (a, b) => a.or(b),
                };
                ("conflicting arguments", args)
            }
        }
        Kind::TooManyValues | Kind::TooFewValues | Kind::WrongNumberOfValues => {
            ("wrong number of values for", named(ContextKind::InvalidArg))
        }
        _ => ("invalid arguments", None),
    };
    let message = match arg {
        Some(a) => format!("{what} {a}"),
        None => what.to_string(),
    };
    // A flag's allowed values are its own, not what was typed.
    match (e.kind(), named(ContextKind::ValidValue)) {
        (Kind::InvalidValue, Some(valid)) if !valid.is_empty() => {
            format!("{message}; expected one of: {valid}")
        }
        _ => message,
    }
}

/// The command a failed parse names, as typed words that are real
/// commands (`claim`, `msg send`), for its help hint; empty for none.
fn help_target(args: &[OsString]) -> String {
    let mut cmd = Cli::command();
    let mut words: Vec<String> = Vec::new();
    let mut rest = args.iter().skip(1).filter_map(|a| a.to_str());
    while let Some(w) = rest.next() {
        match w {
            // After `--` every word is a value, never a command, even
            // where a global flag would take it as its own.
            "--root" | "-r" | "--as" => {
                if rest.next() == Some("--") {
                    break;
                }
            }
            "--" => break,
            w if w.starts_with('-') => {}
            w => match cmd.find_subcommand(w) {
                Some(sub) => {
                    words.push(sub.get_name().to_string());
                    cmd = sub.clone();
                }
                None => break,
            },
        }
    }
    words.join(" ")
}

/// The command a failed parse was for, as its schema names it: the first
/// word that isn't a global flag or its value, with the 0.6.0 spellings
/// mapped to their new names.
fn command_named(args: &[OsString]) -> &'static str {
    let mut words = Vec::new();
    let mut rest = args.iter().skip(1).filter_map(|a| a.to_str());
    while let Some(w) = rest.next() {
        match w {
            "--root" | "-r" | "--as" => {
                rest.next();
            }
            w if w.starts_with('-') => {}
            w => words.push(w),
        }
        if words.len() == 2 {
            break;
        }
    }
    // The 0.6.0 spellings follow their new commands' schemas.
    let all: Vec<&str> = {
        let mut out = Vec::new();
        let mut rest = args.iter().skip(1).filter_map(|a| a.to_str());
        while let Some(w) = rest.next() {
            match w {
                "--root" | "-r" | "--as" => {
                    rest.next();
                }
                // Attached values: --root=x, --as=x, -rx.
                w if w.starts_with("--root=")
                    || w.starts_with("--as=")
                    || (w.starts_with("-r") && !w.starts_with("--")) => {}
                w => out.push(w),
            }
        }
        out
    };
    if let Some(name) = compat::parse_failure_name(&all) {
        return name;
    }
    // `msg send`, `store restore` and the like have a schema per
    // subcommand: any group schema::COMMANDS names as `<group>.<sub>`.
    let grouped = |w: &str| {
        schema::COMMANDS
            .iter()
            .any(|c| c.strip_prefix(w).is_some_and(|r| r.starts_with('.')))
    };
    match (words.first().copied(), words.get(1).copied()) {
        (Some(w), Some(sub)) if grouped(w) => schema::COMMANDS
            .iter()
            .copied()
            .find(|c| c.strip_prefix(w).and_then(|r| r.strip_prefix('.')) == Some(sub))
            .unwrap_or("cli"),
        (Some(w), _) => schema::COMMANDS
            .iter()
            .copied()
            .find(|c| *c == w)
            .unwrap_or("cli"),
        (None, _) => "cli",
    }
}

/// Help or `--version` text, built with the name `mdkg`, as the binary is
/// invoked: the version line, the usage lines, the examples and any
/// `` `mdkg …` `` in the prose.
fn localize_help(text: &str) -> String {
    let bin = BIN.as_str();
    let text = mdkg_format::program::localize(text);
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let named = ["Usage: ", "  ", ""].iter().find_map(|lead| {
            let tail = line.strip_prefix(lead)?.strip_prefix("mdkg ")?;
            Some(format!("{lead}{bin} {tail}"))
        });
        out.push_str(named.as_deref().unwrap_or(line));
    }
    out
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    mdkg_format::program::set_from_arg0(args.first().map(OsString::as_os_str));
    // Clap names the binary from argv[0] in usage lines: the checked name,
    // so an argv[0] carrying odd text can't reach the help.
    let parsed: Vec<OsString> = std::iter::once(OsString::from(BIN.as_str()))
        .chain(args.iter().skip(1).cloned())
        .collect();
    let cli = match Cli::try_parse_from(&parsed) {
        Ok(cli) => cli,
        // Help and --version, named with the binary as invoked.
        Err(e) if !e.use_stderr() => {
            if BIN == mdkg_format::program::DEFAULT {
                e.print().ok();
            } else {
                print!("{}", localize_help(&e.render().to_string()));
            }
            return ExitCode::SUCCESS;
        }
        // Text mode too: clap's own text quotes what was typed, which
        // the output contract forbids (it may be a secret).
        Err(e) => {
            let json = args.iter().any(|a| a == "--json");
            let command = command_named(&args);
            let message = usage_message(&e);
            let help = match help_target(&args) {
                t if t.is_empty() => format!("run `{BIN} --help`"),
                t => format!("run `{BIN} {t} --help`"),
            };
            let d = Diagnostic::error(Code::UsageInvalidArguments, message).with_hint(help);
            return early_failure(command, json, d, 1);
        }
    };
    if let Command::ReceiverQueueGuard { spec, owner } = &cli.command {
        return ExitCode::from(mdkg_run::receiver::guard::run(spec, *owner));
    }
    if cli.allow_store_upgrade {
        mdkg_coord_sqlite::allow_store_upgrades();
    }
    if let Command::Schema { command, out } = &cli.command {
        return ExitCode::from(schema::run(command.as_deref(), out.as_deref()));
    }
    // A bundle verifies with no graph or repository at all.
    if let Command::Verify {
        bundle: Some(b),
        anchor,
        json,
    } = &cli.command
    {
        return ExitCode::from(verify::bundle(b, anchor.as_deref(), *json));
    }
    if let Command::Version { json } = cli.command {
        return ExitCode::from(version::run(json));
    }
    // Claude's apiKeyHelper runs it in the agent's folder: no graph needed.
    if let Command::Auth(a) = &cli.command {
        return ExitCode::from(auth::run(a));
    }
    // A harness runs hooks wherever its session is, graph or not: the hook
    // finds its graph itself and stays silent without one.
    if let Command::Hook(a) = &cli.command {
        return ExitCode::from(hook::run(cli.root.as_deref(), cli.actor.clone(), a));
    }
    let root = match &cli.root {
        Some(r) => std::path::absolute(r).unwrap_or_else(|_| r.clone()),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    // Resolve `..` and symlinks, so node paths come out relative to the
    // root however it was spelled.
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    // Topology discovery below already uses --attr-source. Diagnose an old
    // Git before that discovery, including for install --undo.
    if matches!(cli.command, Command::Install { .. })
        && let Err(e) = doctor::require_git()
    {
        let exit = e.exit_code();
        return early_failure(cli.command.name(), cli.command.json(), *e.diagnostic, exit);
    }
    if cli.command.writes_graph()
        && let Err(e) = graph_repair::require_writable(&root)
    {
        let exit = e.exit_code();
        return early_failure(cli.command.name(), cli.command.json(), *e.diagnostic, exit);
    }
    // Affected reads only committed objects, including its config. Checkout
    // config checks and actor/store discovery must not change that decision.
    if let Command::Affected { base, head, json } = &cli.command {
        return ExitCode::from(affected::run(&root, base, head, *json));
    }
    // MCP mutations enter through this same command dispatch. Hold the graph
    // stable from before config/node reads through their last write and commit.
    // Install owns setup/refresh locks; agents up must not lock out its children.
    let _graph_write = if cli.command.writes_graph()
        && !matches!(
            &cli.command,
            Command::Install { .. } | Command::Agents(agents::AgentsCommand::Up(_))
        ) {
        match graph_repair::lock_writer(&root) {
            Ok(guard) => guard,
            Err(e) => {
                let exit = e.exit_code();
                return early_failure(cli.command.name(), cli.command.json(), *e.diagnostic, exit);
            }
        }
    } else {
        None
    };
    // Setup commands run where there's no graph yet, or a broken one.
    match &cli.command {
        Command::Config(c) => return ExitCode::from(config_cmd::run(&root, c)),
        Command::Init(a) => return ExitCode::from(init::run(&root, a)),
        Command::Doctor(a) => {
            let root = if cli.root.is_some() {
                root.clone()
            } else {
                doctor::find_root(&root)
            };
            return ExitCode::from(doctor::run(&root, a));
        }
        _ => {}
    }
    // Agents start an MCP server wherever their session started, often a
    // subfolder, so `mcp` looks upward for the graph, as git does.
    let root = match (&cli.command, &cli.root) {
        (Command::Mcp, None) => root
            .ancestors()
            .find(|dir| config::has_config(dir))
            .map_or(root.clone(), Path::to_path_buf),
        _ => root,
    };
    // Frozen diagnostics bypass graph config and the coordinator entirely.
    // No empty inventory is returned when the actual inventory wasn't read.
    if matches!(
        &cli.command,
        Command::Status { .. } | Command::Agents(agents::AgentsCommand::Status { .. })
    ) {
        match graph_repair::inspect(&root) {
            Ok(Some(state)) => {
                return ExitCode::from(graph_repair::status(
                    &root,
                    cli.command.name(),
                    cli.command.json(),
                    &state,
                ));
            }
            Ok(None) => (),
            Err(e) => {
                let exit = e.exit_code();
                return early_failure(cli.command.name(), cli.command.json(), *e.diagnostic, exit);
            }
        }
    }
    if !config::has_config(&root) {
        let d = Diagnostic::error(
            Code::ConfigMissing,
            format!("no .mdkg/config.json at {}", root.display()),
        )
        .with_hint(format!(
            "run from the repository root, pass --root <path>, or run `{BIN} init`"
        ));
        return early_failure(cli.command.name(), cli.command.json(), d, 1);
    }
    // A config this build can't read (a newer `schema_version` or format)
    // is refused here, before any command writes a store, a record, a
    // worktree or a plan: a released binary can't learn the refusal later.
    // init, doctor, version, schema, auth and a bundle's verify returned
    // above. A store's verify opens the store, so it's checked too; migrate
    // reads 0.6.0's identity v2 configs as well, and writes its lock first.
    let checked = match cli.command {
        Command::Migrate(_) => config::check_version_for_migrate(&root),
        _ => config::check_version(&root),
    };
    if let Err(e) = checked {
        let exit = e.exit_code();
        return early_failure(cli.command.name(), cli.command.json(), *e.diagnostic, exit);
    }
    // Nobody acts as a human, a role or a turn, in any command:
    // the actor is resolved as every command resolves it (`--as`, then
    // `$MDKG_ACTOR`, then the worktree's folder name) and checked once.
    // As `Repo::actor` resolves it (coord::named_actor): a blank `--as`
    // hides `$MDKG_ACTOR`.
    let named = cli
        .actor
        .clone()
        .or_else(|| std::env::var("MDKG_ACTOR").ok())
        .filter(|a| !a.trim().is_empty());
    // A named actor is used as named; only the default needs the worktree,
    // so a named one costs no git checks here.
    let actor = match &named {
        Some(_) => named.clone(),
        None => coord::Repo::open(&root)
            .ok()
            .map(|repo| repo.actor(cli.actor.clone())),
    };
    // A human acks their own mail, reads their own mailbox, reads threads
    // and leaves rooms (orchestrator, 2026-10-21); those commands check it
    // themselves. Nothing else runs as a human.
    let human_ack = matches!(
        &cli.command,
        Command::Msg(
            msg::MsgCommand::Ack(_)
                | msg::MsgCommand::Inbox(_)
                | msg::MsgCommand::Wait(_)
                | msg::MsgCommand::Thread(_)
        ) | Command::Room(msg::RoomCommand::Leave(_))
    ) && named
        .as_deref()
        .is_some_and(|a| a.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("human:")));
    if !human_ack
        && let Some(actor) = &actor
        && let Err(e) = msg::check_actor(actor)
    {
        let mut d = *e.diagnostic;
        if named.is_none() {
            // Nobody named this actor: it's the worktree's folder name.
            d.message = format!(
                "the actor defaults to this worktree's folder name `{actor}`, which no actor can be"
            );
            d.hint = Some("pass --as <name> or set $MDKG_ACTOR".into());
        } else if cli.actor.is_none() && msg::is_human_name(actor) {
            // A human who exported $MDKG_ACTOR for acks and reads.
            d.hint = Some(format!(
                "{}; for other commands, unset $MDKG_ACTOR (or pass --as '')",
                d.hint.unwrap_or_default()
            ));
        }
        return early_failure(cli.command.name(), cli.command.json(), d, 1);
    }
    if let Some(actor) = &actor {
        mdkg_coord_sqlite::log::set_record_actor(actor);
    }
    ExitCode::from(run(cli, &root))
}
