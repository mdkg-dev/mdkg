//! The 0.6.0 command spellings, kept working for one release while migration
//! rewrites stored command references. Each runs its 0.7.0 equivalent
//! and says so with a `usage.deprecated` notice, which JSON output carries
//! too. 0.6.0 options with no 0.7.0 meaning are accepted and named in the
//! notice; options that would edit fields no 0.7.0 command edits are
//! refused, so nothing is silently dropped.

use std::path::Path;

use clap::{Args, Subcommand};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::GraphError;

use crate::output::{self, Report};
use crate::{NewRest, coord, write};
use mdkg_format::program::BIN;

/// The release the 0.6.0 spellings go away in.
const GONE_IN: &str = "0.8.0";

/// Options 0.6.0 took everywhere that mean nothing in 0.7.0.
#[derive(Debug, Args)]
pub struct Legacy {
    /// 0.6.0: the node's workspace alias; an unqualified target is looked
    /// up in it (`ws:id`).
    #[arg(long, hide = true)]
    ws: Option<String>,
    /// 0.6.0: an event log run id. The coordination log records runs.
    #[arg(long, hide = true)]
    run_id: Option<String>,
    /// 0.6.0: a note for the event log. Post it with `mdkg msg send`.
    #[arg(long, hide = true)]
    note: Option<String>,
}

impl Legacy {
    /// The options it ignores. `--ws` isn't one: it selects the node.
    fn ignored(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.run_id.is_some() {
            out.push("--run-id");
        }
        if self.note.is_some() {
            out.push("--note");
        }
        out
    }
}

/// 0.6.0's field edits, which no 0.7.0 command makes.
#[derive(Debug, Args)]
pub struct FieldEdits {
    #[arg(long, hide = true)]
    priority: Option<i64>,
    #[arg(long, hide = true, value_delimiter = ',')]
    add_artifacts: Vec<String>,
    #[arg(long, hide = true, value_delimiter = ',')]
    add_links: Vec<String>,
    #[arg(long, hide = true, value_delimiter = ',')]
    add_refs: Vec<String>,
    #[arg(long, hide = true, value_delimiter = ',')]
    add_skills: Vec<String>,
    #[arg(long, hide = true, value_delimiter = ',')]
    add_tags: Vec<String>,
    #[arg(long, hide = true, value_delimiter = ',')]
    add_blocked_by: Vec<String>,
    #[arg(long, hide = true)]
    clear_blocked_by: bool,
}

impl FieldEdits {
    fn named(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        let lists = [
            ("--add-artifacts", &self.add_artifacts),
            ("--add-links", &self.add_links),
            ("--add-refs", &self.add_refs),
            ("--add-skills", &self.add_skills),
            ("--add-tags", &self.add_tags),
            ("--add-blocked-by", &self.add_blocked_by),
        ];
        if self.priority.is_some() {
            out.push("--priority");
        }
        out.extend(lists.iter().filter(|(_, v)| !v.is_empty()).map(|(n, _)| *n));
        if self.clear_blocked_by {
            out.push("--clear-blocked-by");
        }
        out
    }
}

#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// 0.6.0 `task start` (now `claim`).
    Start {
        target: String,
        #[command(flatten)]
        legacy: Legacy,
        #[arg(long)]
        json: bool,
    },
    /// 0.6.0 `task update` (now `update`, `claim` or `done`, by status).
    Update {
        target: String,
        #[arg(long)]
        status: Option<String>,
        #[command(flatten)]
        edits: FieldEdits,
        #[command(flatten)]
        legacy: Legacy,
        #[arg(long)]
        json: bool,
    },
    /// 0.6.0 `task done` (now `done`).
    Done {
        target: String,
        /// 0.6.0 took a title; 0.7.0's checkpoint is titled after the task.
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        checkpoint: Option<String>,
        #[arg(long, hide = true)]
        checkpoint_kind: Option<String>,
        #[arg(long, hide = true, value_delimiter = ',')]
        add_artifacts: Vec<String>,
        #[arg(long, hide = true, value_delimiter = ',')]
        add_links: Vec<String>,
        #[arg(long, hide = true, value_delimiter = ',')]
        add_refs: Vec<String>,
        #[command(flatten)]
        legacy: Legacy,
        #[arg(long)]
        json: bool,
    },
}

impl TaskCommand {
    pub fn json(&self) -> bool {
        match self {
            TaskCommand::Start { json, .. }
            | TaskCommand::Update { json, .. }
            | TaskCommand::Done { json, .. } => *json,
        }
    }

    /// The schema the command's output follows: its 0.7.0 command's.
    pub fn name(&self) -> &'static str {
        match self {
            TaskCommand::Start { .. } => "claim",
            TaskCommand::Update { status, .. } => update_route(status.as_deref()),
            TaskCommand::Done { .. } => "done",
        }
    }
}

#[derive(Debug, Args)]
pub struct CheckpointNew {
    #[command(flatten)]
    pub rest: NewRest,
    /// 0.6.0: a template name. 0.7.0 uses the checkpoint template.
    #[arg(long, hide = true)]
    template: Option<String>,
    #[command(flatten)]
    legacy: Legacy,
}

#[derive(Debug, Subcommand)]
pub enum CheckpointCommand {
    /// 0.6.0 `checkpoint new` (now `new checkpoint`).
    New(CheckpointNew),
}

impl CheckpointCommand {
    pub fn json(&self) -> bool {
        match self {
            CheckpointCommand::New(c) => c.rest.json,
        }
    }
}

impl Legacy {
    /// `--ws`, lowercased as 0.6.0 compared workspace aliases.
    fn ws(&self) -> Option<String> {
        self.ws.as_deref().map(str::to_lowercase)
    }
}

/// Says the spelling is deprecated, naming its replacement and any 0.6.0
/// options it ignored.
fn deprecated(old: &str, new: &str, ignored: &[&str]) {
    let mut message =
        format!("`{BIN} {old}` is now `{BIN} {new}`; the 0.6.0 spelling goes away in {GONE_IN}");
    if !ignored.is_empty() {
        message.push_str(&format!(
            "; ignored: {} (no 0.7.0 meaning)",
            ignored.join(", ")
        ));
    }
    output::notice(
        Diagnostic::warning(Code::UsageDeprecated, message)
            .with_hint(format!("run `{BIN} {new}` instead")),
    );
}

/// Refuses an alias, in its 0.7.0 command's envelope.
fn refuse(command: &str, json: bool, e: GraphError) -> u8 {
    output::emit::<()>(command, json, Report::failed(e), |_| {})
}

fn edits_refused(edits: &[&str]) -> GraphError {
    GraphError::usage(
        Code::UsageBadValue,
        format!("{} edit node fields, which no 0.7.0 command does", edits.join(", ")),
    )
    .with_hint("edit the node file's frontmatter and commit it; the 0.6.0 spelling runs only what 0.7.0 has")
}

pub fn task(root: &Path, actor: Option<String>, command: TaskCommand) -> u8 {
    let name = command.name();
    match command {
        TaskCommand::Start {
            target,
            legacy,
            json,
        } => {
            deprecated("task start", "claim", &legacy.ignored());
            let target = match resolve(root, "node", &target, legacy.ws().as_deref()) {
                Ok(node) => node.qid,
                Err(e) => return refuse(name, json, e),
            };
            coord::claim(root, actor, &target, coord::ClaimOptions::exact(), json)
        }
        TaskCommand::Update {
            target,
            status,
            edits,
            legacy,
            json,
        } => {
            let named = edits.named();
            if !named.is_empty() {
                return refuse(name, json, edits_refused(&named));
            }
            let ignored = legacy.ignored();
            let target = match resolve(root, "node", &target, legacy.ws().as_deref()) {
                Ok(node) => node.qid,
                Err(e) => return refuse(name, json, e),
            };
            match status.as_deref().map(str::to_lowercase).as_deref() {
                Some("progress") => {
                    deprecated("task update --status progress", "claim", &ignored);
                    coord::claim(root, actor, &target, coord::ClaimOptions::exact(), json)
                }
                Some("done") => {
                    deprecated("task update --status done", "done", &ignored);
                    write::done(root, actor, &target, false, json)
                }
                Some(s @ ("todo" | "backlog")) => refuse(
                    name,
                    json,
                    GraphError::usage(
                        Code::UsageBadStatus,
                        format!("in 0.7.0, {s} is a lasting status, kept in the node file"),
                    )
                    .with_hint(format!(
                        "to stop working, `{BIN} release <id>` (and `{BIN} update <id> --clear`); \
                         to change it, edit the file"
                    )),
                ),
                other => {
                    deprecated("task update", "update", &ignored);
                    write::update(root, actor, &target, other, false, json)
                }
            }
        }
        TaskCommand::Done {
            target,
            checkpoint,
            checkpoint_kind,
            add_artifacts,
            add_links,
            add_refs,
            legacy,
            json,
        } => {
            let mut named = Vec::new();
            for (n, v) in [
                ("--add-artifacts", &add_artifacts),
                ("--add-links", &add_links),
                ("--add-refs", &add_refs),
            ] {
                if !v.is_empty() {
                    named.push(n);
                }
            }
            if !named.is_empty() {
                return refuse(name, json, edits_refused(&named));
            }
            let mut ignored = legacy.ignored();
            if checkpoint.as_deref().is_some_and(|t| !t.is_empty()) {
                ignored.push("the --checkpoint title");
            }
            if checkpoint_kind.is_some() {
                ignored.push("--checkpoint-kind");
            }
            deprecated("task done", "done", &ignored);
            let target = match resolve(root, "node", &target, legacy.ws().as_deref()) {
                Ok(node) => node.qid,
                Err(e) => return refuse(name, json, e),
            };
            write::done(root, actor, &target, checkpoint.is_some(), json)
        }
    }
}

pub fn checkpoint(root: &Path, actor: Option<String>, command: CheckpointCommand) -> u8 {
    let CheckpointCommand::New(c) = command;
    if c.legacy.ws.is_some() {
        return refuse(
            "new",
            c.rest.json,
            GraphError::usage(
                Code::UsageBadValue,
                "--ws picks a workspace for the new checkpoint, and `new` writes to the graph's own",
            )
            .with_hint("drop --ws, or create the checkpoint in that workspace's graph"),
        );
    }
    let mut ignored = c.legacy.ignored();
    if c.template.is_some() {
        ignored.push("--template");
    }
    deprecated("checkpoint new", "new checkpoint", &ignored);
    let json = c.rest.json;
    write::new_node(root, actor, c.rest.spec("checkpoint"), json)
}

/// The command `task update --status <status>` runs.
fn update_route(status: Option<&str>) -> &'static str {
    match status.map(str::to_lowercase).as_deref() {
        Some("progress") => "claim",
        Some("done") => "done",
        _ => "update",
    }
}

/// Whether a handle names a goal by its id (`goal-…`, `ws:goal-…`, any
/// case): how a 0.6.0 spelling's schema is named before the graph is read.
fn looks_like_goal(handle: &str) -> bool {
    handle
        .rsplit(':')
        .next()
        .is_some_and(|id| id.to_ascii_lowercase().starts_with("goal-"))
}

/// The schema a `goal claim` spelling's output follows: `goal` when it
/// claims a goal's next item, `claim` when it claims a work item.
pub fn goal_claim_name(args: &[String]) -> &'static str {
    match args {
        [] => "goal.claim",
        [one] if looks_like_goal(one) => "goal.claim",
        _ => "claim",
    }
}

/// The schema a 0.6.0 spelling's output follows, from its raw words (for
/// a parse failure, before any command runs): `task start` claims, `task
/// update` by its `--status`, `goal claim` by its operands.
pub fn parse_failure_name(words: &[&str]) -> Option<&'static str> {
    let status = || {
        words.iter().enumerate().find_map(|(i, w)| {
            w.strip_prefix("--status=").or_else(|| {
                (*w == "--status")
                    .then(|| words.get(i + 1).copied())
                    .flatten()
            })
        })
    };
    let operands: Vec<String> = words
        .iter()
        .skip(2)
        .filter(|w| !w.starts_with('-'))
        .map(|w| (*w).to_string())
        .collect();
    match words {
        ["task", "start", ..] => Some("claim"),
        ["task", "update", ..] => Some(update_route(status())),
        ["task", "done", ..] => Some("done"),
        ["checkpoint", ..] => Some("new"),
        ["goal", "claim", ..] => Some(goal_claim_name(&operands)),
        _ => None,
    }
}

/// Resolves a handle to one node of the graph at `root`, as 0.6.0 did:
/// an unqualified id is looked for in `ws` when given. Every alias target
/// goes through here, so nothing but a node (never a path, a role or a
/// turn) reaches the command it runs.
fn resolve(
    root: &Path,
    label: &str,
    handle: &str,
    ws: Option<&str>,
) -> Result<mdkg_graph::IndexNode, GraphError> {
    let config = mdkg_graph::config::load(root)?;
    let index = mdkg_graph::index::build(root, &config)?;
    resolve_in(&index, label, handle, ws)
}

fn resolve_in(
    index: &mdkg_graph::Index,
    label: &str,
    handle: &str,
    ws: Option<&str>,
) -> Result<mdkg_graph::IndexNode, GraphError> {
    match mdkg_graph::resolve::resolve_qid(index, handle, ws) {
        mdkg_graph::resolve::Resolution::Ok(qid) => index.get(&qid).cloned().ok_or_else(|| {
            GraphError::not_found(Code::NodeNotFound, format!("no {label} named that"))
        }),
        other => Err(mdkg_graph::resolve::resolve_error(
            label, handle, &other, ws,
        )),
    }
}

/// 0.6.0's `goal claim [goal] [work]`: with a work item, claims it, after
/// checking it's actionable inside the goal when one is named; with a goal
/// or nothing, claims the goal's next item.
pub fn goal_claim(root: &Path, actor: Option<String>, args: &[String], json: bool) -> u8 {
    match args {
        [] => {
            deprecated("goal claim", "goal next, then claim", &[]);
            write::goal(root, actor, write::GoalAction::Claim(None), json)
        }
        [one] => {
            let node = match resolve(root, "node", one, None) {
                Ok(n) => n,
                Err(e) => return refuse(goal_claim_name(args), json, e),
            };
            if node.node_type == "goal" {
                deprecated("goal claim", "goal next, then claim", &[]);
                write::goal(root, actor, write::GoalAction::Claim(Some(node.qid)), json)
            } else {
                deprecated("goal claim <work>", "claim <work>", &[]);
                coord::claim(root, actor, &node.qid, coord::ClaimOptions::exact(), json)
            }
        }
        [goal, work, ..] => {
            let checked = (|| -> Result<String, GraphError> {
                let config = mdkg_graph::config::load(root)?;
                let index = mdkg_graph::index::build(root, &config)?;
                let goal = resolve_in(&index, "goal", goal, None)?;
                // A node given where a goal is needed (D2).
                if goal.node_type != "goal" {
                    return Err(GraphError::usage(
                        Code::GoalNotGoal,
                        format!(
                            "the first of two operands must be a goal, not a {}",
                            goal.node_type
                        ),
                    ));
                }
                // In the goal's workspace, as 0.6.0 looked for it.
                let work = resolve_in(&index, "work item", work, Some(&goal.ws))?;
                let scope = mdkg_graph::graph_checks::goal_scope(&index, &goal);
                if !scope.actionable.contains(&work.qid) {
                    return Err(GraphError::usage(
                        Code::UsageBadValue,
                        format!("{} isn't actionable work inside {}", work.qid, goal.qid),
                    )
                    .with_hint(format!("`{BIN} goal next <goal>` names its next item")));
                }
                Ok(work.qid)
            })();
            match checked {
                Ok(qid) => {
                    deprecated("goal claim <goal> <work>", "claim <work>", &[]);
                    coord::claim(root, actor, &qid, coord::ClaimOptions::exact(), json)
                }
                Err(e) => refuse("claim", json, e),
            }
        }
    }
}
