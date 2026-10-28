//! Manage committed agent definitions and supervise their turns.
//!
//! `agents add`, `edit` and `rm` manage `.mdkg/agents/<name>.json`.
//! `agents up` watches for mail; explicit names with `--once` run a manual
//! pass. Runtime state and interactive session registrations stay local.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use clap::{Args, Subcommand};
use mdkg_format::{Code, Diagnostic, Severity};
use mdkg_graph::GraphError;
use mdkg_run::orchestration::agent::{self, Definition};
use mdkg_run::policy::Policy;
use mdkg_run::state;
use mdkg_run::supervise::{self, Event, Guard, Supervised, Up};
use mdkg_run::{TurnReport, receiver};
use serde::Serialize;
use serde_json::Value;

use crate::coord::Repo;
use crate::output::{self, DiagnosticOut, Envelope, Report, schema_id};
use crate::run::{self, AgentArgs, HarnessArg, RoleArg, graph_error};
use mdkg_format::program::BIN;

#[derive(Debug, Subcommand)]
pub enum AgentsCommand {
    /// Create a committed agent definition in .mdkg/agents/.
    Add(Box<DefinitionArgs>),
    /// Update an agent definition; fields not given stay as they are.
    Edit(Box<DefinitionArgs>),
    /// Remove a committed definition, keeping local sessions and history.
    Rm {
        agent: String,
        #[arg(long)]
        json: bool,
    },
    /// Wake agents on their mail and run their turns under their policies,
    /// until stopped.
    #[command(after_help = examples!("agents up", "agents up --once", "agents up worker-1 --max-turns 3"))]
    Up(UpArgs),
    /// Show the agents, their policies, and the threads the loop guard
    /// paused.
    #[command(after_help = examples!("agents status", "agents status --json"))]
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Let a thread the loop guard paused run again, or all of an agent's.
    #[command(after_help = examples!("agents resume worker-1", "agents resume worker-1 --thread <thread>"))]
    Resume {
        /// The agent's name (its actor id).
        agent: String,
        /// Clear only this loop-guard thread. Without it, an interactive
        /// receiver also waits for a new primary prompt to select its route.
        #[arg(long)]
        thread: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

impl AgentsCommand {
    pub fn name(&self) -> &'static str {
        match self {
            AgentsCommand::Add(_) => "agents.add",
            AgentsCommand::Edit(_) => "agents.edit",
            AgentsCommand::Rm { .. } => "agents.rm",
            AgentsCommand::Up(_) => "agents.up",
            AgentsCommand::Status { .. } => "agents.status",
            AgentsCommand::Resume { .. } => "agents.resume",
        }
    }

    pub fn json(&self) -> bool {
        match self {
            AgentsCommand::Add(a) | AgentsCommand::Edit(a) => a.json,
            AgentsCommand::Rm { json, .. } => *json,
            AgentsCommand::Up(a) => a.json,
            AgentsCommand::Status { json } | AgentsCommand::Resume { json, .. } => *json,
        }
    }
}

#[derive(Debug, Args)]
pub struct DefinitionArgs {
    /// The actor name and definition filename, without .json.
    pub agent: String,
    /// Read a complete definition from this JSON file before applying flags.
    #[arg(long)]
    pub file: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub harness: Option<HarnessArg>,
    #[arg(long, value_enum)]
    pub role: Option<RoleArg>,
    #[arg(long, conflicts_with = "worktree")]
    pub cwd: Option<PathBuf>,
    /// Give a worker its own worktree (the default without --cwd).
    #[arg(long, conflicts_with = "cwd")]
    pub worktree: bool,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
    #[arg(long)]
    pub base: Option<String>,
    #[arg(long)]
    pub instructions: Option<String>,
    /// Per-agent turn policy, as a JSON object.
    #[arg(long)]
    pub policy: Option<String>,
    /// The actor receiving turn receipts (default: orchestrator).
    #[arg(long)]
    pub reports_to: Option<String>,
    /// Reserved remote definition reference; this release does not run it.
    #[arg(long)]
    pub extends: Option<String>,
    /// Cross-harness preset (auto; other values are reserved).
    #[arg(long)]
    pub mode: Option<String>,
    /// Harness-native sandbox override.
    #[arg(long)]
    pub sandbox: Option<String>,
    /// Harness-native approvals override.
    #[arg(long)]
    pub approvals: Option<String>,
    /// Remove an optional field; repeat for several fields.
    #[arg(long, value_parser = ["extends", "cwd", "worktree", "model", "effort", "base", "instructions", "policy", "allow", "protect", "reports_to", "mode", "sandbox", "approvals"])]
    pub unset: Vec<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DefinitionData {
    pub agent: String,
    pub path: PathBuf,
    /// The commit holding the definition change (current HEAD for a no-op).
    pub commit: String,
    pub changesets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<Definition>,
}

/// Definition commands use the graph writer's commit/changeset path, with
/// an exact file lease so another staged definition can never be absorbed.
fn record_definition(
    root: &Path,
    actor: Option<String>,
    name: &str,
    action: &str,
    change: impl FnOnce(&Repo, &Path) -> Result<Option<Definition>, GraphError>,
) -> Result<Report<DefinitionData>, GraphError> {
    use crate::coord::{self, now_ms, store_error};
    use mdkg_coord::{Command, Mode, Outcome};
    use mdkg_coord_sqlite::git;

    crate::graph_repair::require_writable(root)?;
    let repo = Repo::open(root)?;
    let path = agent::path(&repo.root, name).map_err(graph_error)?;
    let relative = format!(".mdkg/agents/{name}.json");
    crate::anchor::refuse_linked_folders(&repo.root, &relative)?;
    let scope = repo.leaf(&relative)?;
    coord::check_scope_text(&scope)?;
    let actor = repo.actor(actor);
    crate::msg::check_actor_mailbox(&actor, true)?;
    let held = coord::require_authority_hold(&repo, &actor, &scope, None)?;
    let mut store = repo.store()?;
    let now = now_ms();
    repo.observe(&mut store, now)?;
    let state = store.coordinator().map_err(store_error)?;
    let existing = coord::covering_lease(&state, &actor, &scope);
    let taken = existing.is_none();
    let lease = if let Some(lease) = existing {
        if lease.scope != scope || lease.mode != Mode::Exclusive {
            return Err(GraphError::conflict(
                Code::CoordLeaseHeld,
                "definition changes need an exclusive lease on the definition file",
            )
            .with_hint("release the covering or shared lease, then retry"));
        }
        lease.id.clone()
    } else {
        let mut capped = false;
        let outcome = store.execute_with(|state| {
            if coord::at_lease_cap(state, &actor, now) {
                capped = true;
                return Err(mdkg_coord_sqlite::StoreError::Schema("capped".into()));
            }
            Ok(Command::Claim {
                owner: actor.clone(),
                scope: scope.clone(),
                mode: Mode::Exclusive,
                now,
                request: None,
                node: None,
            })
        });
        if capped {
            return Err(coord::lease_cap_error());
        }
        match outcome.map_err(store_error)? {
            Outcome::Lease { lease } => lease,
            Outcome::Refused { error } => return Err(coord::refusal(&error)),
            _ => return Err(bad_agent(name, "definition lease wasn't granted")),
        }
    };
    let result = (|| {
        // Read only under the lease, without following a definition symlink.
        let before = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {
                Some(std::fs::read(&path).map_err(|e| bad_agent(name, e))?)
            }
            Ok(_) => return Err(bad_agent(name, "definition must be a regular file")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(bad_agent(name, e)),
        };
        let definition = change(&repo, &path)?;
        let mut committed_error = false;
        let recorded = (|| {
            // Exercise the same crash boundary as node writes. Returned
            // failures roll back below; a process exit leaves D12's observed
            // state and operator recovery guidance intact.
            crate::write::fault("definition-written")?;
            // Removing a hand-created, untracked definition changes no Git
            // content. Like a no-op edit, report HEAD without a new commit.
            if definition.is_none()
                && git::files_under(&repo.top, scope.as_str())
                    .map_err(coord::git_error)?
                    .is_empty()
            {
                let head = git::head(&repo.top).map_err(coord::git_error)?;
                let outcome = store
                    .execute(&Command::Release {
                        owner: actor.clone(),
                        lease: lease.clone(),
                        now,
                    })
                    .map_err(store_error)?;
                if let Outcome::Refused { error } = outcome {
                    return Err(coord::refusal(&error));
                }
                return Ok((head.commit, Vec::new()));
            }
            crate::write::commit_and_record(
                &repo,
                &mut store,
                &actor,
                &[(lease.to_string(), scope.clone())],
                &[],
                &format!("{action} agent {name}"),
                now,
                None,
            )
            .map(|r| (r.commit, r.changesets))
            .map_err(|(error, committed)| {
                // A recorded Git commit must never be undone, even when the
                // coordinator could not record it. Precommit errors roll back.
                if committed {
                    committed_error = true;
                }
                error
            })
        })();
        let (commit, changesets) = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                if !committed_error {
                    let restored = match &before {
                        Some(bytes) => crate::write::atomic_write(&path, bytes),
                        None => std::fs::remove_file(&path),
                    };
                    if let Err(restore) = restored {
                        return Err(bad_agent(
                            name,
                            format!(
                                "{}; restoring definition failed: {restore}",
                                error.diagnostic.message
                            ),
                        ));
                    }
                }
                return Err(error);
            }
        };
        Ok(Report::ok(DefinitionData {
            agent: name.into(),
            path,
            commit,
            changesets,
            definition,
        })
        .with(coord::release_at_authority(
            &repo,
            &actor,
            &scope,
            held.as_ref(),
        )))
    })();
    if result.is_err() && taken {
        let _ = store.execute(&Command::Release {
            owner: actor,
            lease,
            now,
        });
    }
    result
}

fn definition_report(
    root: &Path,
    actor: Option<String>,
    a: &DefinitionArgs,
    edit: bool,
) -> Result<Report<DefinitionData>, GraphError> {
    record_definition(
        root,
        actor,
        &a.agent,
        if edit { "Edit" } else { "Add" },
        |repo, path| {
            let exists = std::fs::symlink_metadata(path).is_ok();
            if !edit && exists {
                return Err(bad_agent(
                    &a.agent,
                    "definition already exists; use agents edit",
                ));
            }
            let mut value = if let Some(file) = &a.file {
                serde_json::to_value(agent::read(file, &a.agent).map_err(graph_error)?)
                    .map_err(|e| bad_agent(&a.agent, e))?
            } else if edit {
                if exists {
                    serde_json::to_value(agent::read(path, &a.agent).map_err(graph_error)?)
                        .map_err(|e| bad_agent(&a.agent, e))?
                } else {
                    let cfg = mdkg_run::orchestration::load_agent_config(&repo.root)
                        .map_err(graph_error)?;
                    cfg.get("agents")
                        .and_then(|v| v.get(&a.agent))
                        .cloned()
                        .ok_or_else(|| bad_agent(&a.agent, "no definition to edit"))?
                }
            } else {
                serde_json::json!({})
            };
            let object = value
                .as_object_mut()
                .ok_or_else(|| bad_agent(&a.agent, "must be an object"))?;
            object.insert("schema".into(), Value::String(agent::SCHEMA.into()));
            for field in &a.unset {
                object.remove(field);
            }
            if a.worktree {
                object.remove("cwd");
            }
            if a.cwd.is_some() {
                object.remove("worktree");
            }
            let mut set = |key: &str, value: Option<Value>| {
                if let Some(value) = value {
                    object.insert(key.into(), value);
                }
            };
            set(
                "harness",
                a.harness.map(|h| {
                    Value::String(
                        match h {
                            HarnessArg::Claude => "claude",
                            HarnessArg::Codex => "codex",
                            HarnessArg::OpenCode => "opencode",
                        }
                        .into(),
                    )
                }),
            );
            set(
                "role",
                a.role.map(|r| {
                    Value::String(
                        match r {
                            RoleArg::Worker => "worker",
                            RoleArg::Reviewer => "reviewer",
                        }
                        .into(),
                    )
                }),
            );
            for (key, value) in [
                ("model", &a.model),
                ("effort", &a.effort),
                ("base", &a.base),
                ("instructions", &a.instructions),
                ("reports_to", &a.reports_to),
                ("extends", &a.extends),
                ("mode", &a.mode),
                ("sandbox", &a.sandbox),
                ("approvals", &a.approvals),
            ] {
                set(key, value.clone().map(Value::String));
            }
            set(
                "cwd",
                a.cwd
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()
                    .map_err(|e| bad_agent(&a.agent, e))?,
            );
            set("worktree", a.worktree.then_some(Value::Bool(true)));
            if let Some(policy) = &a.policy {
                set(
                    "policy",
                    Some(serde_json::from_str(policy).map_err(|e| bad_agent(&a.agent, e))?),
                );
            }
            let definition = Definition::parse(&a.agent, value).map_err(graph_error)?;
            agent::write(&repo.root, &a.agent, &definition, edit && exists).map_err(graph_error)?;
            Ok(Some(definition))
        },
    )
}

fn remove_definition(
    root: &Path,
    actor: Option<String>,
    name: &str,
) -> Result<Report<DefinitionData>, GraphError> {
    record_definition(root, actor, name, "Remove", |repo, path| {
        let meta = std::fs::symlink_metadata(path).map_err(|e| bad_agent(name, e))?;
        if !meta.is_file() {
            return Err(bad_agent(name, "definition must be a regular file"));
        }
        // Removing a file must not silently reactivate an older inline entry.
        let cfg_path = repo.root.join(".mdkg/config.json");
        let raw: Value =
            serde_json::from_slice(&std::fs::read(&cfg_path).map_err(|e| bad_agent(name, e))?)
                .map_err(|e| bad_agent(name, e))?;
        if raw.get("agents").and_then(|v| v.get(name)).is_some() {
            return Err(bad_agent(
                name,
                "remove the deprecated inline config entry first",
            ));
        }
        std::fs::remove_file(path).map_err(|e| bad_agent(name, e))?;
        Ok(None)
    })
}

#[derive(Debug, Args)]
pub struct UpArgs {
    /// The agents to run (default: configured agents, else saved agents,
    /// plus interactive registrations without a headless spec).
    pub agents: Vec<String>,
    /// Run what's waiting now, then stop.
    #[arg(long)]
    pub once: bool,
    /// Stop each agent after this many turns.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub max_turns: Option<u64>,
    /// How long each wait lasts before starting again, in seconds.
    #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
    pub wait: u64,
    /// Each agent's first turn starts a new session.
    #[arg(long)]
    pub fresh: bool,
    /// Instructions for a manual pass. Requires names and --once.
    #[arg(long)]
    pub prompt: Option<String>,
    /// The reply kind for a manual pass.
    #[arg(long)]
    pub reply_kind: Option<String>,
    /// Override the most messages shown in each turn.
    #[arg(long, allow_negative_numbers = true)]
    pub limit: Option<i64>,
    /// Override the turn timeout in seconds.
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Keep each turn's logs after importing its reply.
    #[arg(long)]
    pub keep_logs: bool,
    /// Print the planned command and prompt. Requires names and --once.
    #[arg(long)]
    pub dry_run: bool,
    /// The mdkg binary the agents' MCP servers run (default: this one, by
    /// its bare name when `PATH` finds this same file, else by its path as
    /// invoked).
    #[arg(long)]
    pub binary: Option<PathBuf>,
    /// OpenCode only: allow its Zen models (throwaway smoke turns).
    #[arg(long)]
    pub allow_zen: bool,
    /// Print one JSON envelope per event, a line each.
    #[arg(long)]
    pub json: bool,
}

/// One event of `agents up`, as its JSON lines carry it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UpEvent {
    pub agent: String,
    /// `turn`, `wake`, `paused`, `manual` or `trouble`.
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake: Option<receiver::Report>,
    /// Threads the loop guard paused, with mail waiting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paused: Vec<String>,
    /// Seconds until the soonest paused thread runs again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_secs: Option<u64>,
}

/// One agent in `agents status`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AgentStatus {
    pub name: String,
    /// Named in the config's `agents`.
    pub configured: bool,
    /// Has saved machine-local runtime state.
    pub known: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<mdkg_run::Harness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<mdkg_run::Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub turns: u64,
    pub policy: Policy,
    /// Threads the loop guard pauses now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paused: Vec<String>,
    /// Why this agent's config or policy can't run, if it can't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// Its allow rules that are never passed to its harness: unreadable,
    /// or letting mdkg run with open arguments, each with what to allow
    /// instead. Read against this binary; a turn reads them against the
    /// `--binary` it runs, and its report names any it drops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_allow: Vec<mdkg_run::profile::DroppedAllow>,
    /// The account its turns would bill now, from its harness's own login
    /// check and the Keychain's list (no key is read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<mdkg_run::auth::AuthStatus>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StatusData {
    pub agents: Vec<AgentStatus>,
    /// The central record of agent threads, every harness's, newest first:
    /// interactive sessions their hooks keep (working while in a turn, idle
    /// after it), and threads `agents attach` keeps while it runs.
    pub threads: Vec<mdkg_run::hooks::Thread>,
    pub receivers: Vec<ReceiverStatus>,
    /// Registered launcher and MCP processes whose build-record locks are held.
    pub processes: Vec<mdkg_run::builds::Process>,
    /// Mixed builds, changed executables, or unreadable process build records.
    pub build_warnings: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReceiverStatus {
    pub actor: String,
    pub route: Option<receiver::route::Route>,
    pub problem: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ResumeData {
    pub agent: String,
    /// The threads let run again.
    pub resumed: Vec<String>,
    pub waiting_for_prompt: bool,
}

/// The parsed `.mdkg/config.json` at the graph root; `null` when missing.
fn config(repo: &Repo) -> Result<Value, GraphError> {
    let cfg = mdkg_run::orchestration::load_agent_config(&repo.root).map_err(graph_error)?;
    if cfg
        .get("agents")
        .and_then(Value::as_object)
        .is_some_and(|agents| {
            agents
                .iter()
                .any(|(name, entry)| name != "defaults" && entry.get("schema").is_none())
        })
    {
        eprintln!(
            "warning: inline agents in .mdkg/config.json are deprecated; use .mdkg/agents/<name>.json (see docs/agent-definitions.md)"
        );
    }
    Ok(cfg)
}

/// The agents the config names, in its order.
fn configured(cfg: &Value) -> Vec<String> {
    cfg.get("agents")
        .and_then(Value::as_object)
        .map(|a| a.keys().filter(|k| *k != "defaults").cloned().collect())
        .unwrap_or_default()
}

/// The agents with saved runtime state in the local run folder.
fn known(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .filter_map(|e| {
                    let f = e.file_name().to_string_lossy().into_owned();
                    let stem = f.strip_suffix(".json")?.replace("%3A", ":");
                    state::check_name(&stem).is_ok().then_some(stem)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn bad_agent(name: &str, what: impl std::fmt::Display) -> GraphError {
    GraphError::usage(
        Code::UsageInvalidArguments,
        format!("agents.{name}: {what}"),
    )
    .with_hint(format!(
        "see `{BIN} agents --help`, and \"Waking agents\" in docs/agents.md"
    ))
}

/// Execution settings resolved from an agent definition.
fn run_args(
    name: &str,
    entry: Option<&Value>,
    a: &UpArgs,
    new: bool,
) -> Result<AgentArgs, GraphError> {
    let mut r = AgentArgs {
        agent: name.to_string(),
        harness: None,
        role: None,
        worktree: false,
        cwd: None,
        model: None,
        effort: None,
        mode: None,
        sandbox: None,
        approvals: None,
        reports_to: None,
        allow: Vec::new(),
        protect: Vec::new(),
        base: None,
        fresh: a.fresh,
        binary: a.binary.clone(),
        prompt: a.prompt.clone(),
        reply_kind: a.reply_kind.clone(),
        limit: a.limit.unwrap_or(20),
        timeout: a.timeout,
        keep_logs: a.keep_logs,
        dry_run: a.dry_run,
        allow_zen: a.allow_zen,
        definition: entry.and_then(|e| e.get("schema")).is_some(),
    };
    let Some(entry) = entry else {
        return Ok(r);
    };
    let Some(o) = entry.as_object() else {
        return Err(bad_agent(name, "must be an object"));
    };
    const KEYS: &[&str] = &[
        "schema",
        "extends",
        "reports_to",
        "mode",
        "sandbox",
        "approvals",
        "harness",
        "role",
        "model",
        "effort",
        "allow",
        "protect",
        "base",
        "cwd",
        "worktree",
        "instructions",
        "policy",
    ];
    if let Some(k) = o.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(bad_agent(name, format!("no field {k:?}")));
    }
    if o.get("extends").is_some_and(|v| !v.is_null()) {
        return Err(bad_agent(
            name,
            "extends is reserved; remote definitions are not implemented",
        ));
    }
    // An existing agent runs as it is: its config fields made it.
    if !new && !r.definition {
        return Ok(r);
    }
    let text = |key: &str| -> Result<Option<String>, GraphError> {
        match o.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(bad_agent(name, format!("{key} must be a string"))),
        }
    };
    let list = |key: &str| -> Result<Vec<String>, GraphError> {
        match o.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(v)) => v
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(String::from)
                        .ok_or_else(|| bad_agent(name, format!("{key} must list strings")))
                })
                .collect(),
            Some(_) => Err(bad_agent(name, format!("{key} must be a list"))),
        }
    };
    r.harness = match text("harness")?.as_deref() {
        None => None,
        Some("claude") => Some(HarnessArg::Claude),
        Some("codex") => Some(HarnessArg::Codex),
        Some("opencode") => Some(HarnessArg::OpenCode),
        Some(h) => return Err(bad_agent(name, format!("no harness {h:?}"))),
    };
    r.role = match text("role")?.as_deref() {
        None => None,
        Some("worker") => Some(RoleArg::Worker),
        Some("reviewer") => Some(RoleArg::Reviewer),
        Some(x) => return Err(bad_agent(name, format!("no role {x:?}"))),
    };
    r.model = text("model")?;
    r.effort = text("effort")?;
    r.mode = text("mode")?;
    r.sandbox = text("sandbox")?;
    r.approvals = text("approvals")?;
    r.reports_to = text("reports_to")?;
    if let Some(e) = &r.effort
        && !mdkg_run::state::EFFORTS.contains(&e.as_str())
    {
        return Err(bad_agent(name, format!("no effort {e:?}")));
    }
    r.allow = list("allow")?;
    r.protect = list("protect")?;
    r.base = text("base")?;
    r.cwd = text("cwd")?.map(PathBuf::from);
    r.worktree = match o.get("worktree") {
        None | Some(Value::Null) => r.cwd.is_none() && matches!(r.role, Some(RoleArg::Worker)),
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad_agent(name, "worktree must be true or false")),
    };
    if r.worktree && r.cwd.is_some() {
        return Err(bad_agent(name, "takes cwd or worktree, not both"));
    }
    Ok(r)
}

/// An agent's standing instructions (`agents.<name>.instructions`), which
/// every automatic or manual turn shows before its mail.
pub(crate) fn instructions(
    name: &str,
    entry: Option<&Value>,
) -> Result<Option<String>, GraphError> {
    match entry.and_then(|e| e.get("instructions")) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(bad_agent(name, "instructions must be a string")),
    }
}

fn check_interactive_flags(name: &str, a: &UpArgs) -> Result<(), GraphError> {
    if let Some(flag) = [
        (a.fresh, "--fresh"),
        (a.dry_run, "--dry-run"),
        (a.prompt.is_some(), "--prompt"),
        (a.reply_kind.is_some(), "--reply-kind"),
        (a.keep_logs, "--keep-logs"),
        (a.allow_zen, "--allow-zen"),
        (a.binary.is_some(), "--binary"),
    ]
    .into_iter()
    .find_map(|(given, flag)| given.then_some(flag))
    {
        return Err(bad_agent(
            name,
            format!("{flag} requires a headless definition"),
        ));
    }
    Ok(())
}

/// Everything `up` runs, made ready: each agent made if it's new, its
/// policy read. Refused whole if any one can't run.
fn ready(
    root: &Path,
    actor: Option<String>,
    a: &UpArgs,
) -> Result<(PathBuf, Vec<Supervised>, Vec<mdkg_run::CliMail>), GraphError> {
    let repo = Repo::open(root)?;
    let sync = mdkg_run::install::worktree::refresh_graph(
        &repo.top,
        &crate::coord::main_ref(&repo.top),
        a.dry_run,
    );
    if let Some(note) = sync.note() {
        // Keep JSON events on stdout intact; startup notes use the same
        // stderr channel as definition-loading warnings.
        eprintln!("{}", mdkg_run::digest::terminal_safe(&note));
    }
    // Even a failed postflight can follow a completed Git fast-forward;
    // recheck what is on disk before preparing any agent state.
    mdkg_graph::config::check_version(&repo.root)?;
    crate::graph_repair::require_writable(&repo.root)?;
    let cfg = config(&repo)?;
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let names = if !a.agents.is_empty() {
        a.agents.clone()
    } else {
        let mut names = configured(&cfg);
        if names.is_empty() {
            names = known(&dir);
        }
        let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
            .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
        for name in receiver::route::actors(&common) {
            if receiver::mode(&repo.root, &dir, &name).map_err(graph_error)?
                == receiver::Mode::Interactive
            {
                names.push(name);
            }
        }
        names.sort();
        names.dedup();
        names
    };
    if names.is_empty() {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "no agents to run: no committed definitions or saved agents",
        )
        .with_hint(format!(
            "create one with `{BIN} agents add <name> --harness <harness> --role <role>`"
        )));
    }
    // Read every selected actor's definition/state and reject unsupported
    // flags before any preparation can save state or take a turn lease.
    // Preparation still rechecks mode, including under the mutation lease.
    for name in &names {
        state::check_name(name).map_err(graph_error)?;
        if receiver::mode(&repo.root, &dir, name).map_err(graph_error)?
            == receiver::Mode::Interactive
        {
            check_interactive_flags(name, a)?;
        }
    }
    let mut agents = Vec::new();
    let mut mails = Vec::new();
    // A queue receipt cannot enforce a cap on model turns. Refuse mixed or
    // interactive watchers with a concurrency cap instead of ignoring it.
    let mut any_interactive = false;

    for name in names {
        let entry = cfg.get("agents").and_then(|g| g.get(&name));
        agent::check_execution(&cfg, &name).map_err(graph_error)?;
        let mut policy = Policy::for_agent(&cfg, &name)
            .map_err(|e| graph_error(e.with_hint(format!("in agents.{name}"))))?;
        policy.check_supported(&name).map_err(graph_error)?;
        if let Some(timeout) = a.timeout {
            policy.timeout = Some(Duration::from_secs(timeout));
        }
        if let Some(limit) = a.limit {
            policy.shape = mdkg_run::policy::Shape::Batch {
                max: limit as usize,
            };
        }
        let new = state::load(&dir, &name).map_err(graph_error)?.is_none();
        // No spec means interactive intent, even before the first hook.
        // Never prepare_watch here: it would save a headless spec.
        if receiver::mode(&repo.root, &dir, &name).map_err(graph_error)?
            == receiver::Mode::Interactive
        {
            any_interactive = true;
            check_interactive_flags(&name, a)?;
            let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
                .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
            let opts = mdkg_run::TurnOptions {
                graph_root: Some(repo.root.clone()),
                common_dir: Some(common.clone()),
                trusted_worktree: Some(repo.top.clone()),
                launcher_root: Some(repo.top.clone()),
                mdkg: run::server_binary(a.binary.as_deref())?,
                ..Default::default()
            };
            let mut mail = mdkg_run::CliMail::new(
                run::this_binary(),
                repo.root.clone(),
                actor.clone().unwrap_or_else(|| name.clone()),
            );
            mail.trusted_common = Some(common);
            mail.trusted_worktree = Some(repo.top.clone());
            agents.push(Supervised { name, policy, opts });
            mails.push(mail);
            continue;
        }
        let mut args = run_args(&name, entry, a, new)?;
        // A relative `cwd` is from the launcher's checkout, wherever
        // `agents up` was started.
        if let Some(c) = &args.cwd
            && c.is_relative()
        {
            args.cwd = Some(repo.top.join(c));
        }
        // Setup can create a saved spec. Check intent before that mutation,
        // under the same lease used by interactive dispatch and reset.
        let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
            .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
        let mut admission_mail = mdkg_run::CliMail::new(
            run::this_binary(),
            repo.root.clone(),
            actor.clone().unwrap_or_else(|| name.clone()),
        );
        admission_mail.trusted_common = Some(common);
        admission_mail.trusted_worktree = Some(repo.top.clone());
        let w = if a.dry_run || !new {
            run::prepare_watch(root, actor.clone(), &args, false)?
        } else {
            receiver::with_mode(
                &mut admission_mail,
                &repo.root,
                &dir,
                &name,
                &repo.top,
                |mode, _| {
                    if mode != receiver::Mode::Headless {
                        return Err(mdkg_run::RunError::failure(
                            "run.unknown_agent",
                            "actor became interactive; restart agents up",
                        ));
                    }
                    Ok(run::prepare_watch(root, actor.clone(), &args, true))
                },
            )
            .map_err(graph_error)??
        };
        let mut opts = w.opts;
        opts.auth = policy.auth;
        opts.limit = policy.batch_max();
        opts.timeout = policy.timeout.or(opts.timeout);
        opts.require_headless = true;
        agents.push(Supervised { name, policy, opts });
        mails.push(w.mail);
    }
    if any_interactive && supervise::cap(&agents).is_some() {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "interactive queue receivers cannot enforce a model-turn concurrency cap; run them separately without that cap",
        ));
    }
    Ok((dir, agents, mails))
}

fn up(root: &Path, actor: Option<String>, a: &UpArgs) -> u8 {
    if a.limit.is_some_and(|n| n < 1) {
        return failed(
            GraphError::usage(Code::UsageBadLimit, "--limit must be a positive integer"),
            a.json,
        );
    }
    if (a.prompt.is_some() || a.dry_run || a.reply_kind.is_some())
        && (!a.once || a.agents.is_empty())
    {
        return failed(
            GraphError::usage(
                Code::UsageInvalidArguments,
                "--prompt, --dry-run and --reply-kind require agent names and --once",
            ),
            a.json,
        );
    }
    if let Err(e) = run::no_inherited_git_env() {
        return failed(e, a.json);
    }
    let (dir, agents, mails) = match ready(root, actor, a) {
        Ok(r) => r,
        Err(e) => return failed(e, a.json),
    };
    let _process_build = if a.dry_run {
        None
    } else {
        crate::version::hold_process(
            root,
            mdkg_run::builds::Kind::Launcher,
            agents.iter().map(|a| a.name.clone()).collect(),
        )
    };
    // Explicit names with --once are a manual pass. Default discovery still
    // uses the automatic policy, guard and interactive receiver paths.
    if a.once && !a.agents.is_empty() {
        let mut exit = 0;
        for (agent, mut mail) in agents.iter().zip(mails) {
            if receiver::mode(
                agent.opts.graph_root.as_deref().unwrap_or(root),
                &dir,
                &agent.name,
            )
            .is_ok_and(|m| m == receiver::Mode::Interactive)
            {
                let how = Up {
                    once: true,
                    max_turns: a.max_turns,
                    wait_secs: a.wait,
                    pause: Duration::from_secs(5),
                    fresh: a.fresh,
                };
                receiver::supervise(&mut mail, &dir, agent, &how, &|name, event| {
                    emit_event(name, &event, a.json)
                });
                continue;
            }
            let result = (|| {
                let mut saved = match state::load(&dir, &agent.name)? {
                    Some(saved) => saved,
                    None if a.dry_run => {
                        state::resolve(&agent.name, None, agent.opts.flags.clone(), root)?
                    }
                    None => {
                        return Err(mdkg_run::RunError::usage(
                            "run.unknown_agent",
                            "no saved agent",
                        ));
                    }
                };
                let report = mdkg_run::turn::turn(&mut mail, &dir, &mut saved, &agent.opts)?;
                if !a.dry_run
                    && let Some(thread) = &report.thread
                {
                    Guard::update(&dir, &agent.name, true, |guard| {
                        guard.clear(Some(thread));
                        Ok(())
                    })?;
                }
                Ok(report)
            })();
            if let Err(error) = &result {
                exit = graph_error(error.clone()).kind.exit_code();
            }
            emit_event(&agent.name, &Event::Turn(&result), a.json);
        }
        return exit;
    }
    // Each agent's thread takes its own mail client, by its name.
    let names: Vec<String> = agents.iter().map(|s| s.name.clone()).collect();
    let mails = Mutex::new(
        names
            .iter()
            .cloned()
            .zip(mails.into_iter().map(Some))
            .collect::<Vec<_>>(),
    );
    let make = |name: &str| {
        let mut m = mails.lock().unwrap_or_else(|e| e.into_inner());
        m.iter_mut()
            .find(|(n, _)| n == name)
            .and_then(|(_, c)| c.take())
            .expect("one mail client per agent")
    };
    let printing = Mutex::new(());
    let json = a.json;
    let on = |agent: &str, ev: Event<'_>| {
        let _held = printing.lock().unwrap_or_else(|e| e.into_inner());
        emit_event(agent, &ev, json);
    };
    let how = Up {
        wait_secs: a.wait,
        max_turns: a.max_turns,
        once: a.once,
        pause: Duration::from_secs(5),
        fresh: a.fresh,
    };
    if !json {
        println!("agents up: {}", names.join(", "));
    }
    match supervise::up(&make, &dir, &agents, &how, &on) {
        Ok(ran) => {
            if !json {
                let total: u64 = ran.iter().sum();
                println!("{total} turn(s)");
            }
            0
        }
        Err(e) => failed(graph_error(e), json),
    }
}

/// `up` refused: with `--json`, one envelope on one line, as its events.
fn failed(e: GraphError, json: bool) -> u8 {
    if json {
        print_line_failure(*e.diagnostic);
    } else {
        output::eprint_diagnostic(&e.diagnostic);
    }
    e.kind.exit_code()
}

/// A failed `agents up` envelope on one line, as its events are.
pub fn print_line_failure(d: mdkg_format::Diagnostic) {
    let envelope: Envelope<UpEvent> = Envelope {
        schema: schema_id("agents.up"),
        ok: false,
        data: None,
        diagnostics: vec![DiagnosticOut::from(&d)],
    };
    println!("{}", serde_json::to_string(&envelope).unwrap_or_default());
}

fn emit_event(agent: &str, ev: &Event<'_>, json: bool) {
    let safe = mdkg_run::digest::terminal_safe;
    let (data, diag): (UpEvent, Option<GraphError>) = match ev {
        Event::Turn(Ok(r)) => (
            UpEvent {
                agent: agent.into(),
                event: "turn".into(),
                turn: Some(r.clone()),
                wake: None,
                paused: Vec::new(),
                cooldown_secs: None,
            },
            None,
        ),
        Event::Turn(Err(e)) => (
            UpEvent {
                agent: agent.into(),
                event: "turn".into(),
                turn: None,
                wake: None,
                paused: Vec::new(),
                cooldown_secs: None,
            },
            Some(graph_error(e.clone())),
        ),
        Event::Trouble(e) => (
            UpEvent {
                agent: agent.into(),
                event: "trouble".into(),
                turn: None,
                wake: None,
                paused: Vec::new(),
                cooldown_secs: None,
            },
            Some(graph_error((*e).clone())),
        ),
        Event::Paused(threads, cooldown) => (
            UpEvent {
                agent: agent.into(),
                event: "paused".into(),
                turn: None,
                wake: None,
                paused: threads.to_vec(),
                cooldown_secs: cooldown.map(|c| c.as_secs()),
            },
            None,
        ),
        Event::Wake(report) => (
            UpEvent {
                agent: agent.into(),
                event: "wake".into(),
                turn: None,
                wake: Some((*report).clone()),
                paused: Vec::new(),
                cooldown_secs: None,
            },
            None,
        ),
        Event::Manual => (
            UpEvent {
                agent: agent.into(),
                event: "manual".into(),
                turn: None,
                wake: None,
                paused: Vec::new(),
                cooldown_secs: None,
            },
            None,
        ),
    };
    let waiting = data
        .wake
        .as_ref()
        .is_some_and(|wake| wake.status == receiver::WakeStatus::Waiting)
        .then(|| {
            Diagnostic::new(
                Code::RunUnknownAgent,
                Severity::Info,
                format!("no definition named {agent}; waiting for interactive registration"),
            )
            .with_hint(format!(
                "`{BIN} agents add {agent} --harness <harness> --role <role>` creates a headless agent"
            ))
        });
    if json {
        let envelope = Envelope {
            schema: schema_id("agents.up"),
            ok: diag.is_none(),
            data: Some(&data),
            diagnostics: diag
                .iter()
                .map(|d| DiagnosticOut::from(&*d.diagnostic))
                .chain(waiting.iter().map(DiagnosticOut::from))
                .collect(),
        };
        println!("{}", serde_json::to_string(&envelope).unwrap_or_default());
        return;
    }
    match (&data.turn, &diag) {
        (Some(r), _) => run::print_turn(r),
        (None, Some(d)) => {
            print!("{}: ", safe(agent));
            output::eprint_diagnostic(&d.diagnostic);
        }
        (None, None) if data.wake.is_some() => {
            if let Some(w) = &data.wake {
                println!("{}: receiver {:?}", safe(agent), w.status);
            }
            if let Some(advice) = &waiting {
                output::eprint_diagnostic(advice);
            }
        }
        (None, None) if data.event == "manual" => {
            println!(
                "{}: manual trigger; `{} agents up {} --once` wakes it",
                safe(agent),
                crate::BIN,
                safe(agent)
            );
        }
        (None, None) => println!(
            "{}: loop guard paused {} thread(s) with mail waiting{}; `{BIN} agents resume {}` lets them run",
            safe(agent),
            data.paused.len(),
            data.cooldown_secs
                .map(|s| format!(", the first runs again in {s} s"))
                .unwrap_or_default(),
            safe(agent)
        ),
    }
}

fn status_report(root: &Path) -> Result<StatusData, GraphError> {
    let repo = Repo::open(root)?;
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let live = mdkg_run::builds::live(&dir).unwrap_or_else(|e| mdkg_run::builds::Live {
        problems: vec![e.message],
        ..Default::default()
    });
    let build_warnings = live.warnings(&crate::version::build());
    let cfg = config(&repo)?;
    let conf = configured(&cfg);
    let mdkg = run::this_binary();
    let mut names = conf.clone();
    let receiver_common = mdkg_coord_sqlite::git::common_dir(&repo.top)
        .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
    for k in known(&dir)
        .into_iter()
        .chain(receiver::route::actors(&receiver_common))
    {
        if !names.contains(&k) {
            names.push(k);
        }
    }
    let now = state::now_ms();
    let mut agents = Vec::new();
    for name in names {
        let st = state::load(&dir, &name).ok().flatten();
        let (policy, problem) = match Policy::for_agent(&cfg, &name) {
            Ok(p) => {
                let problem = p.check_supported(&name).err().map(|e| e.message);
                (p, problem)
            }
            Err(e) => (Policy::default(), Some(e.message)),
        };
        let paused = policy
            .loop_guard
            .map(|g| Guard::load(&dir, &name).paused(&g, now))
            .unwrap_or_default();
        // Its harness: as made, or as the config will make it.
        let harness = st.as_ref().map(|s| s.spec.harness).or_else(|| {
            cfg.get("agents")
                .and_then(|a| a.get(&name))
                .and_then(|e| e.get("harness"))
                .and_then(Value::as_str)
                .and_then(mdkg_run::Harness::parse)
        });
        // Where its turns will run: as made, else its configured `cwd`
        // (from the launcher's checkout, as `agents up` reads it), else
        // the launcher's checkout (review of 06792e4).
        let cwd = st.as_ref().map_or_else(
            || {
                cfg.get("agents")
                    .and_then(|a| a.get(&name))
                    .and_then(|e| e.get("cwd"))
                    .and_then(Value::as_str)
                    .map_or_else(|| repo.top.clone(), |c| repo.top.join(c))
            },
            |s| s.spec.cwd.clone(),
        );
        let auth = harness.map(|h| {
            mdkg_run::auth::status(
                h,
                &h.program(),
                &cwd,
                &policy.auth,
                &mdkg_run::auth::KeySource::default(),
            )
        });
        // As made, else the config's `allow`, which will make it: a
        // cloned repository's rules are named before any turn runs
        // (RC1 security audit SEC5).
        let dropped_allow = mdkg_run::profile::dropped_allow(
            &st.as_ref().map_or_else(
                || configured_allow(cfg.get("agents").and_then(|a| a.get(&name))),
                |s| s.spec.allow.clone(),
            ),
            &mdkg,
        );
        agents.push(AgentStatus {
            auth,
            configured: conf.contains(&name),
            known: st.is_some(),
            harness: st.as_ref().map(|s| s.spec.harness),
            role: st.as_ref().map(|s| s.spec.role),
            cwd: st.as_ref().map(|s| s.spec.cwd.clone()),
            turns: st.as_ref().map_or(0, |s| s.turns),
            name,
            policy,
            paused,
            problem,
            dropped_allow,
        });
    }
    let threads = mdkg_coord_sqlite::git::common_dir(&repo.top)
        .map(|c| mdkg_run::hooks::threads(&c))
        .unwrap_or_default();
    let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
        .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
    let receivers = receiver::route::actors(&common)
        .into_iter()
        .map(|actor| {
            let (route, problem) = match receiver::route::snapshot(&common, &actor) {
                Ok(route) => (route, None),
                Err(e) => (None, Some(e.message)),
            };
            ReceiverStatus {
                actor,
                route,
                problem,
            }
        })
        .collect();
    Ok(StatusData {
        agents,
        threads,
        receivers,
        processes: live.processes,
        build_warnings,
    })
}

/// The allow rules an agent's config entry lists; anything that isn't a
/// string is left to `up`, which refuses the entry.
fn configured_allow(entry: Option<&Value>) -> Vec<String> {
    entry
        .and_then(|e| e.get("allow"))
        .and_then(Value::as_array)
        .map(|rules| {
            rules
                .iter()
                .filter_map(|r| r.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn resume_report(root: &Path, agent: &str, thread: Option<&str>) -> Result<ResumeData, GraphError> {
    state::check_name(agent).map_err(graph_error)?;
    let repo = Repo::open(root)?;
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
        .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
    let interactive = receiver::mode(&repo.root, &dir, agent).map_err(graph_error)?
        == receiver::Mode::Interactive;
    if state::load(&dir, agent).map_err(graph_error)?.is_none()
        && !match receiver::route::read(&common, agent) {
            Ok(route) => {
                route.is_some()
                    || (interactive
                        && thread.is_none()
                        && receiver::route::actors(&common)
                            .iter()
                            .any(|name| name == agent))
            }
            // Existence only, never dispatch authority: an explicit complete
            // reset may preserve damaged evidence under the run/route locks.
            Err(_) if interactive && thread.is_none() => receiver::route::actors(&common)
                .iter()
                .any(|name| name == agent),
            Err(e) => return Err(graph_error(e)),
        }
    {
        return Err(
            GraphError::not_found(Code::RunUnknownAgent, "no agent has that name yet")
                .with_hint(format!("`{} agents status` lists the agents", crate::BIN)),
        );
    }
    let clear_guard = || {
        Guard::update(&dir, agent, true, |g| {
            let resumed = match thread {
                Some(t) => g
                    .threads
                    .contains_key(t)
                    .then(|| t.to_string())
                    .into_iter()
                    .collect(),
                None => g.threads.keys().cloned().collect(),
            };
            g.clear(thread);
            Ok(resumed)
        })
    };
    let waiting_for_prompt = interactive && thread.is_none();
    let resumed = if interactive {
        // Serialize reset with the whole queue attempt, not only its route
        // write. A reset cannot revoke the route after reservation but before
        // the dispatcher launches its already-admitted submission.
        let mut mail = mdkg_run::CliMail::new(run::this_binary(), repo.root.clone(), agent.into());
        mail.trusted_common = Some(common.clone());
        mail.trusted_worktree = Some(repo.top.clone());
        receiver::with_mode(&mut mail, &repo.root, &dir, agent, &repo.top, |mode, _| {
            if mode != receiver::Mode::Interactive {
                return Err(mdkg_run::RunError::failure(
                    "run.unknown_agent",
                    "actor mode changed; retry resume",
                ));
            }
            if waiting_for_prompt {
                receiver::route::reset(&common, agent)?;
            }
            clear_guard()
        })
        .map_err(graph_error)?
    } else {
        clear_guard().map_err(graph_error)?
    };
    Ok(ResumeData {
        agent: agent.into(),
        resumed,
        waiting_for_prompt,
    })
}

pub fn run(root: &Path, actor: Option<String>, c: &AgentsCommand) -> u8 {
    let safe = output::shown_path;
    match c {
        AgentsCommand::Add(a) | AgentsCommand::Edit(a) => {
            let edit = matches!(c, AgentsCommand::Edit(_));
            let r = definition_report(root, actor, a, edit).unwrap_or_else(Report::failed);
            output::emit(c.name(), a.json, r, |d| {
                println!(
                    "{}: {} ({})",
                    safe(&d.agent),
                    safe(&d.path.to_string_lossy()),
                    safe(&d.commit)
                )
            })
        }
        AgentsCommand::Rm { agent, json } => {
            let r = remove_definition(root, actor, agent).unwrap_or_else(Report::failed);
            output::emit(c.name(), *json, r, |d| {
                println!(
                    "removed {} ({})",
                    safe(&d.path.to_string_lossy()),
                    safe(&d.commit)
                )
            })
        }
        AgentsCommand::Up(a) => up(root, actor, a),
        AgentsCommand::Status { json } => {
            let r = status_report(root).map_or_else(Report::failed, Report::ok);
            output::emit("agents.status", *json, r, |d| {
                for warning in &d.build_warnings {
                    println!("warning: {}", safe(warning));
                }
                if d.processes.is_empty() && d.build_warnings.is_empty() {
                    println!("no build inventory yet");
                }
                for process in &d.processes {
                    println!(
                        "{:?} pid {}: mdkg {} at {}",
                        process.kind,
                        process.pid,
                        safe(&process.build.version),
                        safe(&process.build.commit)
                    );
                }
                for a in &d.agents {
                    let what = match (a.harness, a.role) {
                        (Some(h), Some(r)) => format!("{} {}", h.as_str(), r.as_str()),
                        _ if !a.known && !a.configured => "interactive receiver".into(),
                        _ => "not made yet".into(),
                    };
                    println!(
                        "{}: {what}, {} turn(s), trigger {}{}",
                        safe(&a.name),
                        a.turns,
                        match a.policy.trigger {
                            mdkg_run::policy::Trigger::Mail => "mail",
                            mdkg_run::policy::Trigger::Manual => "manual",
                            mdkg_run::policy::Trigger::Later(_) => "(later)",
                        },
                        if a.paused.is_empty() {
                            String::new()
                        } else {
                            format!(", {} thread(s) paused", a.paused.len())
                        }
                    );
                    // The rule is the user's own config, escaped for the
                    // terminal.
                    for d in &a.dropped_allow {
                        println!(
                            "  warning: allow rule {} isn't passed to the agent: {}",
                            safe(&d.rule),
                            safe(&d.fix)
                        );
                    }
                    if let Some(auth) = &a.auth {
                        println!(
                            "  auth: {}{}{}",
                            auth.source,
                            auth.method
                                .as_deref()
                                .map(|m| format!(" ({})", safe(m)))
                                .unwrap_or_default(),
                            if auth.api_key_fallback {
                                if auth.key_in_keychain {
                                    ", API key fallback on"
                                } else {
                                    ", API key fallback on but no key in the Keychain"
                                }
                            } else {
                                ""
                            }
                        );
                    }
                    for t in &a.paused {
                        println!("  paused: {}", safe(t));
                    }
                    if let Some(p) = &a.problem {
                        println!("  can't run: {}", safe(p));
                    }
                }
                for receiver in &d.receivers {
                    let state = receiver.problem.as_deref().unwrap_or_else(|| {
                        if d.agents
                            .iter()
                            .any(|a| a.name == receiver.actor && !a.paused.is_empty())
                        {
                            return "paused by loop guard";
                        }
                        match &receiver.route {
                            Some(r) if r.suspended => "suspended; resume and send a primary prompt",
                            Some(r) if r.observation_pending => {
                                "suspended; hook observation incomplete"
                            }
                            Some(r) if r.session.is_none() => "waiting for a primary prompt",
                            Some(r) if r.working => "working",
                            Some(_) => "idle; wakes may continue after the TUI closes",
                            None => "waiting for registration",
                        }
                    });
                    println!("receiver {}: {}", safe(&receiver.actor), safe(state));
                    if let Some(attempt) = receiver
                        .route
                        .as_ref()
                        .and_then(|r| r.last_attempt.as_ref())
                    {
                        println!(
                            "  last attempt through #{}: {:?}",
                            attempt.through, attempt.delivery
                        );
                    }
                }
                if !d.threads.is_empty() {
                    println!("threads:");
                }
                for t in &d.threads {
                    println!(
                        "  {} {} {}: {}{}, {}",
                        safe(&t.actor),
                        safe(&t.harness),
                        safe(&t.session),
                        match t.state {
                            mdkg_run::hooks::ThreadState::Working => "working",
                            mdkg_run::hooks::ThreadState::Idle => "idle",
                        },
                        if t.bound() { ", attached" } else { "" },
                        safe(&t.cwd.display().to_string())
                    );
                }
            })
        }
        AgentsCommand::Resume {
            agent,
            thread,
            json,
        } => {
            let r = resume_report(root, agent, thread.as_deref())
                .map_or_else(Report::failed, Report::ok);
            output::emit("agents.resume", *json, r, |d| {
                println!(
                    "{}: {} thread(s) let run again",
                    safe(&d.agent),
                    d.resumed.len()
                );
                if d.waiting_for_prompt {
                    println!("waiting for the next primary Codex prompt; no route selected");
                }
            })
        }
    }
}
