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

mod logs;
mod stop;
pub use logs::{LogsArgs, LogsData};
pub use stop::{StopArgs, StopData};

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
    /// Stop agents gracefully: a turn in flight finishes, then each
    /// agent's supervisor admits no more of its turns. Nothing is killed.
    /// Inside an agent's turn it may stop only that agent.
    #[command(after_help = examples!("agents stop worker-1 --drain", "agents stop --all --drain --wait 600"))]
    Stop(StopArgs),
    /// Show a turn's output, normalized across harnesses, or follow an
    /// agent's turns as they run.
    #[command(after_help = examples!("agents logs worker-1", "agents logs worker-1 --follow --json", "agents logs worker-1 --follow --after turn:1 --json"))]
    Logs(LogsArgs),
    /// Show the agents, their policies, and the threads the loop guard
    /// paused.
    #[command(after_help = examples!("agents status", "agents status --json"))]
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Bill an agent's next turn, one, on your API key, after its
    /// subscription hit its usage limit. Nothing charges the key on its own;
    /// the agent's policy must allow this (`auth.api_key_fallback`). Refused
    /// inside an agent's turn, and for an agent's own actor: a person runs
    /// it from their own terminal.
    #[command(name = "use-key", after_help = examples!("agents use-key worker-1"))]
    UseKey {
        /// The agent's name (its actor id).
        agent: String,
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
        /// Confirm the current physical store when retaining a foreign record.
        #[arg(long, conflicts_with = "thread")]
        store: Option<String>,
        /// Preserve full recovery context in an immutable archive.
        #[arg(long, conflicts_with = "thread")]
        archive_context: bool,
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
            AgentsCommand::Stop(_) => "agents.stop",
            AgentsCommand::Logs(_) => "agents.logs",
            AgentsCommand::Status { .. } => "agents.status",
            AgentsCommand::Resume { .. } => "agents.resume",
            AgentsCommand::UseKey { .. } => "agents.use-key",
        }
    }

    pub fn json(&self) -> bool {
        match self {
            AgentsCommand::Add(a) | AgentsCommand::Edit(a) => a.json,
            AgentsCommand::Rm { json, .. } => *json,
            AgentsCommand::Up(a) => a.json,
            AgentsCommand::Stop(a) => a.json,
            AgentsCommand::Logs(a) => a.json,
            AgentsCommand::Status { json }
            | AgentsCommand::Resume { json, .. }
            | AgentsCommand::UseKey { json, .. } => *json,
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
    /// The actor receiving turn receipts and faults: turn errors and refused
    /// replies (default: orchestrator).
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
    #[arg(long, value_parser = ["extends", "cwd", "worktree", "model", "effort", "base", "instructions", "policy", "allow", "protect", "reports_to", "mode", "sandbox", "approvals", "follow"])]
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

/// What a definition command does to the definition file.
enum Change {
    /// Write it (`replace`: it exists and is replaced in place).
    Write {
        definition: Box<Definition>,
        replace: bool,
    },
    Remove,
}

/// The bytes [`agent::write`] writes for a definition.
fn definition_bytes(name: &str, definition: &Definition) -> Result<Vec<u8>, GraphError> {
    let mut bytes = serde_json::to_vec_pretty(definition).map_err(|e| bad_agent(name, e))?;
    bytes.push(b'\n');
    if bytes.len() > 1 << 20 {
        return Err(bad_agent(name, "agent definition exceeds 1 MiB"));
    }
    Ok(bytes)
}

/// Definition commands use the graph writer's commit/changeset path, with
/// an exact file lease so another staged definition can never be absorbed.
/// In ref mode the definition is read from the graph cache and published
/// on the graph ref through `GraphGit`'s write path, never the checkout's
/// files or HEAD (edd-01a18867 row 25).
fn record_definition(
    root: &Path,
    actor: Option<String>,
    name: &str,
    action: &str,
    change: impl FnOnce(&Repo, &Path) -> Result<Change, GraphError>,
) -> Result<Report<DefinitionData>, GraphError> {
    use crate::coord::{self, now_ms, store_error};
    use mdkg_coord::{Command, Mode, Outcome};
    use mdkg_coord_sqlite::git;

    // `agents` commands keep the checkout (main.rs); a definition write
    // binds to the graph ref here, which replays pending intents and
    // brings the cache to the tip.
    let bound = coord::graph_git::bind(root)?;
    let root = bound.as_deref().unwrap_or(root);
    crate::graph_repair::require_writable(root)?;
    let repo = Repo::open(root)?;
    let ref_mode = coord::graph_git::for_root(&repo.root);
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
    let release = |store: &mut mdkg_coord_sqlite::Store| -> Result<(), GraphError> {
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
        Ok(())
    };
    let result = (|| {
        let (commit, changesets, definition) = match &ref_mode {
            Some(mode) => {
                use crate::coord::graph_git::{
                    Publish, Wrote, next_request_id, prepare_record_publish,
                };
                let graph = mode.graph.name.clone();
                let mut changesets = Vec::new();
                let wrote = prepare_record_publish(
                    mode,
                    &mut store,
                    (&next_request_id(), "agents", &actor),
                    |w, _| {
                        // Read in the cache at the session's base, under the
                        // lease; nothing is written there but through `w`.
                        let definition = match change(&repo, &path)? {
                            Change::Write { definition, .. } => {
                                w.write(scope.as_str(), &definition_bytes(name, &definition)?)?;
                                Some(*definition)
                            }
                            Change::Remove => {
                                w.delete(scope.as_str())?;
                                None
                            }
                        };
                        crate::write::fault("definition-written")?;
                        Ok((
                            Publish {
                                message: format!("{action} agent {name}"),
                                leases: vec![lease.to_string()],
                                allowed: vec![scope.clone()],
                                trailers: Vec::new(),
                            },
                            definition,
                        ))
                    },
                    |store, prepared| {
                        let source = mdkg_coord::Source {
                            commit: prepared.commit.clone(),
                            branch: graph.clone(),
                        };
                        changesets = crate::write::record_changesets(
                            &repo,
                            store,
                            &actor,
                            &[(lease.to_string(), scope.clone())],
                            &[],
                            &source,
                            now,
                            None,
                            None,
                        )?;
                        Ok(())
                    },
                )?;
                match wrote {
                    Wrote::Done(definition, Some(written)) => {
                        (written.receipt.commit, changesets, definition)
                    }
                    // Nothing changed: no changeset holds the lease.
                    Wrote::Done(definition, None) => {
                        release(&mut store)?;
                        (mode.commit(), Vec::new(), definition)
                    }
                    Wrote::Already(written) => (written.receipt.commit, Vec::new(), None),
                }
            }
            None => {
                // Read only under the lease, without following a definition symlink.
                let before = match std::fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_file() => {
                        Some(std::fs::read(&path).map_err(|e| bad_agent(name, e))?)
                    }
                    Ok(_) => return Err(bad_agent(name, "definition must be a regular file")),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(bad_agent(name, e)),
                };
                let definition = match change(&repo, &path)? {
                    Change::Write {
                        definition,
                        replace,
                    } => {
                        agent::write(&repo.root, name, &definition, replace)
                            .map_err(graph_error)?;
                        Some(*definition)
                    }
                    Change::Remove => {
                        std::fs::remove_file(&path).map_err(|e| bad_agent(name, e))?;
                        None
                    }
                };
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
                        release(&mut store)?;
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
                match recorded {
                    Ok((commit, changesets)) => (commit, changesets, definition),
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
                }
            }
        };
        Ok(Report::ok(DefinitionData {
            agent: name.into(),
            path: path.clone(),
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
        let _ = release(&mut store);
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
            // Edit needs something to edit, `--file` or not: a definition,
            // or a legacy inline entry to copy (review of 26581a80).
            let inline = if edit && !exists {
                // Ref mode: the graph ref's committed config, read
                // through the checkout (the cache is no worktree).
                let cfg = if crate::coord::graph_git::for_root(&repo.root).is_some() {
                    mdkg_run::orchestration::authority(&repo.top)
                        .map_err(graph_error)?
                        .config
                } else if a.file.is_some() {
                    // Only whether the entry exists: `--file` replaces it,
                    // so neither it nor another agent is validated first
                    // (review of 73189c1d).
                    mdkg_run::orchestration::authority(&repo.root)
                        .map_err(graph_error)?
                        .config
                } else {
                    mdkg_run::orchestration::load_agent_config(&repo.root).map_err(graph_error)?
                };
                Some(
                    cfg.get("agents")
                        .and_then(|v| v.get(&a.agent))
                        .cloned()
                        .ok_or_else(|| no_definition(&a.agent, "edit"))?,
                )
            } else {
                None
            };
            let mut value = if let Some(file) = &a.file {
                serde_json::to_value(agent::read(file, &a.agent).map_err(graph_error)?)
                    .map_err(|e| bad_agent(&a.agent, e))?
            } else if let Some(inline) = inline {
                inline
            } else if edit {
                serde_json::to_value(agent::read(path, &a.agent).map_err(graph_error)?)
                    .map_err(|e| bad_agent(&a.agent, e))?
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
            Ok(Change::Write {
                definition: Box::new(definition),
                replace: edit && exists,
            })
        },
    )
}

fn remove_definition(
    root: &Path,
    actor: Option<String>,
    name: &str,
) -> Result<Report<DefinitionData>, GraphError> {
    record_definition(root, actor, name, "Remove", |repo, path| {
        // The config agents are read from: the graph ref's committed one
        // in ref mode (this checkout's may not exist), else this
        // checkout's file (review of 73189c1d).
        let ref_mode = crate::coord::graph_git::for_root(&repo.root).is_some();
        let cfg_path = repo.root.join(".mdkg/config.json");
        let raw = || -> Result<Value, GraphError> {
            if ref_mode {
                return Ok(mdkg_run::orchestration::authority(&repo.top)
                    .map_err(graph_error)?
                    .config);
            }
            serde_json::from_slice(&std::fs::read(&cfg_path).map_err(|e| bad_agent(name, e))?)
                .map_err(|e| bad_agent(name, e))
        };
        // Where to remove an inline entry. `agents edit` would copy it to
        // a file and leave it, so it isn't offered.
        let remove_entry = if ref_mode {
            format!(
                "remove agents.{name} from the graph ref's .mdkg/config.json: edit it in the \
                 registered graph checkout and commit it there"
            )
        } else {
            format!("remove agents.{name} from .mdkg/config.json and commit it")
        };
        let inline = |raw: &Value| raw.get("agents").and_then(|v| v.get(name)).is_some();
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            // No file: say whether a legacy inline entry is what's there
            // (review of 26581a80).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(if inline(&raw()?) {
                    GraphError::not_found(
                        Code::RunUnknownAgent,
                        format!("agents.{name} has no definition file, only a legacy inline entry"),
                    )
                    .with_hint(remove_entry.clone())
                } else {
                    no_definition(name, "remove")
                });
            }
            Err(e) => return Err(bad_agent(name, e)),
        };
        if !meta.is_file() {
            return Err(bad_agent(name, "definition must be a regular file"));
        }
        // Removing a file must not silently reactivate an older inline entry.
        if inline(&raw()?) {
            return Err(
                bad_agent(name, "remove the deprecated inline config entry first")
                    .with_hint(remove_entry),
            );
        }
        Ok(Change::Remove)
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
    /// `turn`, `wake`, `paused`, `manual`, `trouble` or `stopped` (drained
    /// by `agents stop`).
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
    /// Layered file definition for the next preparation; saved state fields
    /// below continue to describe the last prepared agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective: Option<crate::config_cmd::EffectiveAgentData>,
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
    #[serde(serialize_with = "serialize_status_policy")]
    pub policy: Policy,
    /// Threads the loop guard pauses now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paused: Vec<String>,
    /// Why this agent's config or policy can't run, if it can't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// Latest advisory receipt; delivery failure never makes this agent unrunnable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<mdkg_run::turn::receipt::delivery::DeliveryStatus>,
    /// Why the local receipt status could not be read, distinct from no receipt yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_problem: Option<String>,
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
    /// The key, offered or marked: after a usage limit, the `agents use-key`
    /// command to bill the next turn on it; a mark waiting for that turn;
    /// or a mark held because it couldn't be recorded durably.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing: Option<String>,
    /// What a client needs to re-attach (API v1 A18): `stopped`, `idle`,
    /// `running`, `draining` or `paused` (an open set).
    pub state: String,
    /// The supervisor running it, or the last one whose record names it
    /// when that one is gone (`stale`); null when none.
    pub supervisor: Option<mdkg_run::turn::observe::SupervisorView>,
    /// The turn running now; null when none.
    pub current_turn: Option<mdkg_run::turn::observe::CurrentTurn>,
    /// The last turn that isn't running; null when none.
    pub last_turn: Option<mdkg_run::turn::observe::LastTurn>,
    /// The position `agents logs <agent> --follow --after` takes to pick up
    /// from this snapshot: never "from now", so a turn that starts or ends
    /// in between is still delivered.
    pub follow_from: String,
    /// A worker's branch against mainRef (ref mode, G15): absent when
    /// there's no mainRef here, or no worktree yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_base: Option<CodeBase>,
}

/// How a worker's branch stands against the code base it follows.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CodeBase {
    /// mainRef's full name.
    pub main_ref: String,
    /// Commits on the worker's branch that mainRef lacks.
    pub ahead: u64,
    /// Commits on mainRef that the worker's branch lacks.
    pub behind: u64,
}

fn serialize_status_policy<S: serde::Serializer>(
    policy: &Policy,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut value = serde_json::to_value(policy).map_err(serde::ser::Error::custom)?;
    crate::config_cmd::redact_display(&mut value, "/agents/status/policy");
    value.serialize(serializer)
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StatusData {
    /// The event stream's head, read with this snapshot: `msg watch
    /// --after` it to see every change since (A10). Absent without a store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_cursor: Option<String>,
    pub agents: Vec<AgentStatus>,
    pub config_warnings: Vec<String>,
    /// The central record of agent threads, every harness's, newest first:
    /// interactive sessions their hooks keep (working while in a turn, idle
    /// after it), and threads `agents attach` keeps while it runs.
    pub threads: Vec<mdkg_run::hooks::Thread>,
    pub receivers: Vec<ReceiverStatus>,
    /// Registered launcher and MCP processes whose build-record locks are held.
    pub processes: Vec<mdkg_run::builds::Process>,
    /// Mixed builds, changed executables, or unreadable process build records.
    pub build_warnings: Vec<String>,
    /// Durable watches, including interrupted actors without runtime state.
    pub progress: Vec<ProgressStatus>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProgressStatus {
    pub agent: String,
    /// A watch held the existing progress lock during this snapshot.
    pub active: bool,
    /// Display only; the versioned progress record validates independently.
    pub record: Option<Value>,
    pub current_store: Option<String>,
    pub archive: Option<PathBuf>,
    pub problem: Option<String>,
    pub repair: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReceiverStatus {
    pub actor: String,
    pub route: Option<receiver::route::Route>,
    pub problem: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UseKeyData {
    pub agent: String,
    pub harness: mdkg_run::Harness,
    /// The mark the agent's next turn consumes at admission, billing that
    /// one turn on the key.
    pub mark: PathBuf,
    /// Whether the note that wakes the agent's supervisor was sent. It
    /// wakes a supervisor that's running and triggered by mail; otherwise
    /// the key turn runs when the agent next runs. The turn knows it by the
    /// id recorded for it, and sets it aside unseen when it comes from the
    /// launcher's own actor.
    pub note_sent: bool,
    /// Why the note couldn't be sent: the mark stands, and the next mail
    /// wakes the turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_problem: Option<String>,
}

/// `agents use-key`: the agent's harness as made, else as configured, and
/// its policy from the config.
fn use_key(root: &Path, actor: Option<String>, agent: &str) -> Result<UseKeyData, GraphError> {
    state::check_name(agent).map_err(graph_error)?;
    let repo = Repo::open(root)?;
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let snapshot = config(&repo)?;
    let cfg = &snapshot.effective.values;
    let caller = repo.actor(actor);
    // The user's choice, never an agent's (review of 6b4ee963; orchestrator,
    // 2026-10-29, for person-class actions): refused for an agent's actor,
    // under a live turn of this repository anywhere in the process chain
    // (its MCP server included), which `env -i` can't hide, as the git
    // hooks read it, and whenever that chain can't be read; and with the
    // turn mark in the environment, as `abandon` is. A non-agent actor the
    // caller names is accepted only when no turn is found.
    let refuse = |why: String| {
        Err(GraphError::usage(
            Code::RunUnsupported,
            format!("use-key is refused in an agent turn: {why}"),
        ))
    };
    if std::env::var_os(mdkg_run::profile::TURN_MARK).is_some_and(|mark| !mark.is_empty()) {
        return refuse("the turn's mark is in the environment".into());
    }
    // The caller's own saved state, looked up exactly and never read or
    // followed (reviews of 807dde3a and 9989c849): anything at its path, a
    // dangling link or an unreadable file included, refuses, and only its
    // absence counts as no agent. A name no agent could have (`/`, `..`)
    // builds no path at all.
    // Agent names are lowercase, and a case-insensitive filesystem finds
    // `Lane`'s state as `lane`'s: the caller is compared lowercased in
    // both checks (review of ce3e9b4d).
    let probe = caller.to_ascii_lowercase();
    if configured(cfg)
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&caller))
    {
        return refuse(format!("{caller} is an agent"));
    }
    if state::check_name(&probe).is_ok() {
        match std::fs::symlink_metadata(state::state_path(&dir, &probe)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => return refuse(format!("{caller} is an agent")),
            Err(e) => {
                return refuse(format!("can't tell whether {caller} is an agent: {e}"));
            }
        }
    }
    let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
        .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
    // Fails closed: a chain that can't be read counts as inside a turn.
    match crate::hook::git::ancestor_turn_checked(&common) {
        Ok(None) => {}
        Ok(Some(turn)) => return refuse(format!("this runs under {turn}'s turn")),
        Err(why) => {
            return refuse(format!(
                "it can't rule out running inside a turn ({why}); run it from your own terminal"
            ));
        }
    }
    let policy = Policy::for_agent(cfg, agent).map_err(graph_error)?;
    let harness = state::load(&dir, agent)
        .ok()
        .flatten()
        .map(|s| s.spec.harness)
        .or_else(|| {
            cfg.get("agents")
                .and_then(|a| a.get(agent))
                .and_then(|e| e.get("harness"))
                .and_then(Value::as_str)
                .and_then(mdkg_run::Harness::parse)
        })
        .ok_or_else(|| unknown_agent(agent))?;
    let mark = mdkg_run::auth::use_key(&dir, agent, harness, &policy.auth).map_err(graph_error)?;
    // Wakes the agent's supervisor, which waits for mail newer than the
    // limited turn's: a note from you, so the next turn runs and takes up
    // the mail the limit left unread. The mark stands if it can't be sent.
    // Its id is minted and recorded here first, so the turn knows the note
    // by the id, never by its text (orchestrator, 2026-10-29, on #6209).
    let id = uuid::Uuid::now_v7().to_string();
    let sent = mdkg_run::auth::record_wake(&dir, agent, &id)
        .map_err(|e| e.message)
        .and_then(|()| send_wake_note(&repo.root, &caller, agent, &id));
    Ok(UseKeyData {
        agent: agent.to_string(),
        harness,
        mark,
        note_sent: sent.is_ok(),
        note_problem: sent.err(),
    })
}

/// Sends `agent` use-key's wake note as `caller`, with the id use-key
/// recorded for it.
fn send_wake_note(root: &Path, caller: &str, agent: &str, id: &str) -> Result<(), String> {
    let out = std::process::Command::new(run::this_binary())
        .arg("--root")
        .arg(root)
        .args(["--as", caller, "msg", "send", "--kind=note", "--json"])
        .arg(format!("--to={agent}"))
        .arg(format!("--id={id}"))
        .arg(format!("--text={}", mdkg_run::auth::USE_KEY_NOTE))
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("can't run msg send: {e}"))?;
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    if envelope["ok"] == true {
        Ok(())
    } else {
        Err(envelope["diagnostics"][0]["message"].as_str().map_or_else(
            || "msg send failed".to_string(),
            crate::config_cmd::display_text,
        ))
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ResumeData {
    pub agent: String,
    /// The threads let run again.
    pub resumed: Vec<String>,
    pub waiting_for_prompt: bool,
    /// Exact pre-recovery record shown before mutation, retained as context.
    pub progress_before: Option<Value>,
    pub recovery_archive: Option<PathBuf>,
}

/// Capture runtime inputs once; management continues to load authored values.
fn config(repo: &Repo) -> Result<mdkg_run::orchestration::runtime::Snapshot, GraphError> {
    let authority = mdkg_run::orchestration::authority(&repo.root).map_err(graph_error)?;
    config_with(repo, authority)
}

/// The configuration of the graph at `repo`, built from an authority the
/// operation already pinned (R1): committed objects only, never the
/// worktree's files.
fn config_with(
    repo: &Repo,
    authority: mdkg_graph::graphref::Authority,
) -> Result<mdkg_run::orchestration::runtime::Snapshot, GraphError> {
    let snapshot =
        mdkg_run::orchestration::runtime::load_with(&repo.root, authority).map_err(graph_error)?;
    if !snapshot.authored_inline_agents.is_empty() {
        eprintln!(
            "warning: inline agents in .mdkg/config.json are deprecated; use .mdkg/agents/<name>.json (see docs/agent-definitions.md)"
        );
    }
    Ok(snapshot)
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

/// A name no definition, saved agent or registration has: nothing to act
/// on, exit 3 (e2e B12).
pub(crate) fn unknown_agent(name: &str) -> GraphError {
    GraphError::not_found(Code::RunUnknownAgent, format!("no agent named {name}"))
        .with_hint(format!("`{BIN} agents status` lists the agents"))
}

/// No definition of `name` to `what` (edit, remove): nothing to act on,
/// exit 3. The agent may still be saved or registered, so this names the
/// definition, not the agent (review of 26581a80).
fn no_definition(name: &str, what: &str) -> GraphError {
    GraphError::not_found(
        Code::RunUnknownAgent,
        format!("no definition named {name} to {what}"),
    )
    .with_hint(format!(
        "`{BIN} agents status` lists the agents; `{BIN} agents add {name} --harness <harness> \
         --role <role>` creates a definition"
    ))
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
        // Read where `agents up` sets each worker's follow options.
        "follow",
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
fn graph_id(repo: &Repo) -> Result<uuid::Uuid, GraphError> {
    // Ref mode: the graph ref's committed config; a checkout holds none.
    let id = if graph_ref(repo)?.is_some() {
        mdkg_run::orchestration::authority(&repo.root)
            .map_err(graph_error)?
            .graph_id
    } else {
        mdkg_graph::config::load(&repo.root)?
            .graph_id
            .ok_or_else(|| bad_agent("launcher", "graph_id is required for durable progress"))?
            .to_string()
    };
    uuid::Uuid::parse_str(&id).map_err(|e| bad_agent("launcher", e))
}

/// The clone's graph-ref settings: `None` in checkout mode.
fn graph_ref(repo: &Repo) -> Result<Option<mdkg_graph::graphref::GraphRef>, GraphError> {
    mdkg_graph::graphref::resolve(&repo.top).map_err(|e| {
        GraphError::usage(
            Code::parse(e.code).unwrap_or(Code::RunCwdOutside),
            format!("can't read the graph-ref settings: {}", e.message),
        )
    })
}

fn check_invocation(
    record: &supervise::progress::Record,
    invocation: &str,
) -> Result<(), GraphError> {
    record.require_ready().map_err(graph_error)?;
    if !record.finished && record.invocation != invocation {
        return Err(bad_agent(
            &record.agent,
            format!(
                "unfinished supervisor invocation differs; inspect agents status, then run `{BIN} agents resume {}` to retain context and retire it",
                record.agent
            ),
        ));
    }
    Ok(())
}

pub(crate) fn shell_word(value: &str) -> String {
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

fn recovery_up(prefix: &str, name: &str, a: &UpArgs) -> String {
    let mut words = vec![prefix.to_string(), "agents up".into(), shell_word(name)];
    for (flag, enabled) in [
        ("--once", a.once),
        ("--fresh", a.fresh),
        ("--keep-logs", a.keep_logs),
        ("--allow-zen", a.allow_zen),
        ("--json", a.json),
    ] {
        if enabled {
            words.push(flag.into());
        }
    }
    for (flag, value) in [
        ("--max-turns", a.max_turns.map(|v| v.to_string())),
        ("--wait", Some(a.wait.to_string())),
        ("--prompt", a.prompt.clone()),
        ("--reply-kind", a.reply_kind.clone()),
        ("--limit", a.limit.map(|v| v.to_string())),
        ("--timeout", a.timeout.map(|v| v.to_string())),
        (
            "--binary",
            a.binary.as_ref().map(|p| p.to_string_lossy().into_owned()),
        ),
    ] {
        if let Some(value) = value {
            words.push(format!("{flag} {}", shell_word(&value)));
        }
    }
    words.join(" ")
}

/// A partial group finalization cannot authorize a fresh pass for its finished
/// members. No cohort record is needed: refuse mixed snapshots of this selected
/// invocation, both before preparation and again while every lock is held.
fn check_group_start(
    states: &[(&str, &str, Option<&supervise::progress::Record>)],
    root: &Path,
    actor: &str,
    a: &UpArgs,
) -> Result<(), GraphError> {
    let matching: Vec<_> = states
        .iter()
        .filter_map(|(_, invocation, record)| record.filter(|r| r.invocation == *invocation))
        .collect();
    if !matching.iter().any(|r| r.finished) || !matching.iter().any(|r| !r.finished) {
        return Ok(());
    }
    let state_lines = states
        .iter()
        .map(|(name, invocation, record)| match record {
            Some(r) => format!(
                "{name}: {}, count {}, turn {}, receiver {}{}",
                if r.finished { "finished" } else { "unfinished" },
                r.progress.ran,
                r.current.as_ref().map_or("none", |t| t.id.as_str()),
                r.current_receiver
                    .as_ref()
                    .map_or("none", |t| t.attempt_id.as_str()),
                if r.invocation == *invocation {
                    ""
                } else {
                    " (different invocation)"
                }
            ),
            None => format!("{name}: no saved watch"),
        })
        .collect::<Vec<_>>()
        .join("; ");
    let prefix = format!(
        "{BIN} --root {} --as {}",
        shell_word(&root.to_string_lossy()),
        shell_word(actor)
    );
    let mut repair = vec![format!("Inspect: {prefix} agents status")];
    for record in matching.into_iter().filter(|r| !r.finished) {
        let resume = format!("{prefix} agents resume {}", shell_word(&record.agent));
        if record.current.is_some() || record.current_receiver.is_some() {
            repair.push(format!("{resume}; inspect the retained interruption metadata and Git/mail state before intentionally starting new work"));
        } else if a.once && !a.agents.is_empty() && record.progress.ran > 0 {
            repair.push(format!("{resume}; this retires the already charged manual pass; recovery is complete without another agents up for {}", record.agent));
        } else {
            repair.push(format!(
                "{resume}; then rerun only this unfinished agent with the same prompt: {}",
                recovery_up(&prefix, &record.agent, a)
            ));
        }
    }
    repair.push("Do not rerun finished agents as recovery. A later whole-group start or other changed selection/invocation is intentional new work.".into());
    Err(graph_error(mdkg_run::RunError::failure("run.progress_invalid", format!(
        "selected invocation has mixed finished and unfinished watches; startup refused: {state_lines}"
    )).with_hint(repair.join("\n"))))
}

struct Ready {
    dir: PathBuf,
    agents: Vec<Supervised>,
    mails: Vec<mdkg_run::CliMail>,
    progress: Vec<supervise::progress::Locked>,
    /// The committed definitions a long-running watch reloads on.
    definitions: supervise::reload::Definitions,
    /// The coordinator store, whose versions it also watches.
    store: Option<PathBuf>,
}

fn ready(root: &Path, actor: Option<String>, a: &UpArgs) -> Result<Ready, GraphError> {
    let repo = Repo::open(root)?;
    let graph_ref = graph_ref(&repo)?;
    // The code base workers start from: optional in ref mode, the branch
    // work settles on in checkout mode (so its invocations hash as before).
    let main_ref = match &graph_ref {
        Some(graph) => graph.main_ref_base.clone(),
        None => Some(crate::coord::main_ref(&repo.top)),
    };
    match &graph_ref {
        // Ref mode (R18): from any checkout, with no `.worktrees/graph` and
        // no main. Pending intents are replayed and the cache brought to
        // the tip, the catch-up every graph command makes; `agents up`
        // keeps the checkout as its root, so the binding ends here.
        Some(_) => {
            if !a.dry_run {
                crate::coord::graph_git::bind(root)?;
                crate::coord::graph_git::unbind();
            }
        }
        // Checkout mode, unchanged: the graph checkout follows main.
        None => {
            let main = main_ref.as_deref().unwrap_or("main");
            let sync = mdkg_run::install::worktree::refresh_graph(&repo.top, main, a.dry_run);
            if let Some(note) = sync.note() {
                eprintln!("{}", mdkg_run::digest::terminal_safe(&note));
            }
            mdkg_graph::config::check_version(&repo.root)?;
            crate::graph_repair::require_writable(&repo.root)?;
        }
    }
    // One graph authority read (R1) builds the configuration and is what the
    // watch compares against, so a definition published while this loads
    // still reloads, and nothing changes between a check and the read. It's
    // committed objects only: an uncommitted edit never enters, in a first
    // start or a re-exec, and a re-exec names one.
    let token = supervise::reload::ExecToken::inherited().filter(|_| !a.dry_run);
    let authority = mdkg_run::orchestration::authority(&repo.root).map_err(graph_error)?;
    if graph_ref.is_some() {
        // The committed config's version, as `bind` checks it.
        mdkg_graph::config::from_snapshot(authority.config.clone())?;
    }
    let snapshot = config_with(&repo, authority.clone())?;
    for warning in &snapshot.warnings {
        eprintln!(
            "warning: {}",
            mdkg_run::digest::terminal_safe(&crate::config_cmd::display_text(&warning.to_string()))
        );
    }
    let cfg = &snapshot.effective.values;
    let graph = graph_id(&repo)?;
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
        .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
    let mut names = if !a.agents.is_empty() {
        a.agents.clone()
    } else {
        let mut names = configured(cfg);
        if names.is_empty() {
            names = known(&dir);
        }
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
    // A re-exec leaves out the agents `agents stop` drained before it.
    if token.is_some()
        && let Some((_, stopped)) = supervise::registry::inherited()
    {
        names.retain(|n| !stopped.contains(n));
    }
    if names.is_empty() {
        return Err(bad_agent(
            "launcher",
            "no agents to run: no committed definitions or saved agents",
        ));
    }
    // Lock acquisition and fingerprints use a stable order. Duplicate names
    // cannot silently share a budget, mail client, or lock.
    names.sort();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(bad_agent("launcher", "duplicate selected agent"));
    }
    let definitions = supervise::reload::Definitions::new(
        &repo.top,
        &repo.root,
        &repo.prefix,
        &names,
        &authority,
    );
    struct Plan {
        name: String,
        policy: Policy,
        headless: Option<(AgentArgs, run::WatchPlan)>,
        invocation: String,
    }
    let mut plans = Vec::new();
    let mut snapshots = Vec::new();
    for name in &names {
        state::check_name(name).map_err(graph_error)?;
        agent::check_execution(cfg, name).map_err(graph_error)?;
        let mut policy = Policy::for_agent(cfg, name).map_err(graph_error)?;
        policy.check_supported(name).map_err(graph_error)?;
        if let Some(timeout) = a.timeout {
            policy.timeout = Some(Duration::from_secs(timeout));
        }
        if let Some(limit) = a.limit {
            policy.shape = mdkg_run::policy::Shape::Batch {
                max: limit as usize,
            };
        }
        let interactive = receiver::mode(&repo.root, &dir, name).map_err(graph_error)?
            == receiver::Mode::Interactive;
        let entry = cfg.get("agents").and_then(|g| g.get(name));
        let headless = if interactive {
            check_interactive_flags(name, a)?;
            // A pass over a name nothing defines, saves, registers or
            // has sent mail to would only report it waiting: a typo, so
            // it fails (e2e B12). Mail waiting for it, supervisor
            // progress to recover, or a watch, still waits for the
            // registration. A store or progress folder that can't be
            // read counts as something waiting.
            if a.once
                && !configured(cfg).contains(name)
                && !known(&dir).contains(name)
                && !receiver::route::actors(&common).contains(name)
                && supervise::progress::actors(&dir).is_ok_and(|saved| !saved.contains(name))
                && repo
                    .store()
                    .ok()
                    .and_then(|store| store.unacked(std::slice::from_ref(name)).ok())
                    .is_some_and(|unacked| unacked.is_empty())
            {
                return Err(unknown_agent(name));
            }
            None
        } else {
            let new = state::load(&dir, name).map_err(graph_error)?.is_none();
            let mut args = run_args(name, entry, a, new)?;
            if let Some(cwd) = &args.cwd
                && cwd.is_relative()
            {
                args.cwd = Some(repo.top.join(cwd));
            }
            let plan = run::plan_watch(root, actor.clone(), &args, cfg)?;
            Some((args, plan))
        };
        let spec = headless.as_ref().map(|(_, p)| {
            let mut spec = p.agent.spec.clone();
            spec.pinned = None;
            spec
        });
        // Effective semantics, not argv, the running build, session, or Git
        // pins. Initial materialization must not change the restart identity.
        let value = serde_json::json!({
            "schema": "mdkg.watch-invocation.v1", "selected": names,
            "agent": name, "interactive": interactive, "spec": spec,
            "policy": policy, "instructions": instructions(name, entry)?,
            "root": repo.root, "launcher": repo.actor(actor.clone()),
            "main_ref": main_ref,
            "once": a.once, "manual": a.once && !a.agents.is_empty(),
            "max_turns": a.max_turns, "wait": a.wait, "fresh": a.fresh,
            "prompt": a.prompt, "reply_kind": a.reply_kind,
            "keep_logs": a.keep_logs, "allow_zen": a.allow_zen,
            "binary_override": a.binary,
        });
        let invocation = mdkg_format::hash::Digest::of_record(&value)
            .map_err(|e| bad_agent(name, e))?
            .hex()
            .to_string();
        snapshots.push(if a.dry_run {
            None
        } else {
            supervise::progress::inspect(&dir, graph, name).map_err(graph_error)?
        });
        plans.push(Plan {
            name: name.clone(),
            policy,
            headless,
            invocation,
        });
    }
    // Re-execed after a reload: each agent's own unfinished watch continues
    // under the committed definitions read above (adopted under its lock
    // below). An uncommitted edit didn't enter them; it's only named.
    if token.is_some() {
        if let Some(path) = definitions.differs() {
            eprintln!(
                "warning: {} in this worktree differs from the graph authority's ({}); this restarted watch runs the authority's definitions",
                mdkg_run::digest::terminal_safe(&path),
                mdkg_run::digest::terminal_safe(definitions.source())
            );
        } else if let Some(path) = definitions.dirty() {
            eprintln!(
                "warning: {} has uncommitted changes; this restarted watch runs the graph authority's definitions",
                mdkg_run::digest::terminal_safe(&path)
            );
        }
    }
    let adopts = |name: &str, record: &supervise::progress::Record| {
        !record.finished
            && token
                .as_ref()
                .and_then(|t| t.watch(name))
                .is_some_and(|w| w == record.watch_id)
    };
    for (p, record) in plans.iter().zip(&mut snapshots) {
        if let Some(record) = record
            && adopts(&p.name, record)
            && record.invocation != p.invocation
        {
            record.invocation = p.invocation.clone();
        }
    }
    let states: Vec<_> = plans
        .iter()
        .zip(&snapshots)
        .map(|(p, record)| (p.name.as_str(), p.invocation.as_str(), record.as_ref()))
        .collect();
    check_group_start(&states, &repo.root, &repo.actor(actor.clone()), a)?;
    for (p, record) in plans.iter().zip(&snapshots) {
        if let Some(record) = record {
            check_invocation(record, &p.invocation)?;
        }
    }
    if plans.iter().any(|p| p.headless.is_none())
        && plans.iter().any(|p| p.policy.concurrency.is_some())
    {
        return Err(bad_agent(
            "launcher",
            "interactive queue receivers cannot enforce a model-turn concurrency cap; run them separately without that cap",
        ));
    }
    // Recheck every record under all locks before publishing any start or agent
    // state. Every worker keeps its lock until the entire invocation drains.
    let mut progress = Vec::new();
    if !a.dry_run {
        for p in &plans {
            let saved =
                supervise::progress::Locked::open(&dir, graph, &p.name).map_err(graph_error)?;
            progress.push(saved);
        }
        for (p, saved) in plans.iter().zip(&mut progress) {
            if let Some(watch) = token.as_ref().and_then(|t| t.watch(&p.name))
                && saved.record().is_some_and(|r| adopts(&p.name, r))
            {
                saved.adopt(&p.invocation, watch).map_err(graph_error)?;
            }
        }
        let states: Vec<_> = plans
            .iter()
            .zip(&progress)
            .map(|(p, saved)| (p.name.as_str(), p.invocation.as_str(), saved.record()))
            .collect();
        check_group_start(&states, &repo.root, &repo.actor(actor.clone()), a)?;
        for (p, saved) in plans.iter().zip(&progress) {
            if let Some(record) = saved.record() {
                check_invocation(record, &p.invocation)?;
            }
        }
        for (p, saved) in plans.iter().zip(&mut progress) {
            saved.start(&p.invocation, None).map_err(graph_error)?;
        }
    }
    let mut agents = Vec::new();
    let mut mails = Vec::new();
    for p in plans {
        let mut mail = mdkg_run::CliMail::new(
            run::this_binary(),
            repo.root.clone(),
            actor.clone().unwrap_or_else(|| p.name.clone()),
        );
        mail.trusted_common = Some(common.clone());
        mail.trusted_worktree = Some(repo.top.clone());
        let (opts, mail) = if let Some((args, plan)) = p.headless {
            let new = state::load(&dir, &p.name).map_err(graph_error)?.is_none();
            let w = if a.dry_run || !new {
                run::apply_watch(plan, &args, false)?
            } else {
                receiver::with_mode(
                    &mut mail,
                    &repo.root,
                    &dir,
                    &p.name,
                    &repo.top,
                    |mode, _| {
                        if mode != receiver::Mode::Headless {
                            return Err(mdkg_run::RunError::failure(
                                "run.unknown_agent",
                                "actor became interactive; restart agents up",
                            ));
                        }
                        Ok(run::apply_watch(plan, &args, true))
                    },
                )
                .map_err(graph_error)??
            };
            let mut opts = w.opts;
            opts.definition_commit = supervise::reload::definition_commit(
                &repo.top,
                &repo.prefix,
                &p.name,
                &authority.commit,
            );
            opts.auth = p.policy.auth;
            opts.limit = p.policy.batch_max();
            opts.timeout = p.policy.timeout.or(opts.timeout);
            opts.require_headless = true;
            // Ref mode: workers follow mainRef by notes before each turn
            // (G15); `follow: "merge"` is 0.7.2's, and says so.
            if let Some(graph) = &graph_ref
                && let Some(main) = &graph.main_ref_base
            {
                let merge = cfg
                    .get("agents")
                    .and_then(|g| g.get(&p.name))
                    .and_then(|e| e.get("follow"))
                    .is_some_and(|f| f == "merge");
                opts.follow = Some(mdkg_run::turn::follow::Follow {
                    main_ref: main.clone(),
                    merge,
                });
            }
            (opts, w.mail)
        } else {
            (
                mdkg_run::TurnOptions {
                    graph_root: Some(repo.root.clone()),
                    common_dir: Some(common.clone()),
                    trusted_worktree: Some(repo.top.clone()),
                    launcher_root: Some(repo.top.clone()),
                    mdkg: run::server_binary(a.binary.as_deref())?,
                    ..Default::default()
                },
                mail,
            )
        };
        agents.push(Supervised {
            name: p.name,
            policy: p.policy,
            opts,
        });
        mails.push(mail);
    }
    Ok(Ready {
        dir,
        agents,
        mails,
        progress,
        definitions,
        store: mdkg_coord_sqlite::git::store_path(&repo.top).ok(),
    })
}

fn up(root: &Path, actor: Option<String>, a: &UpArgs) -> u8 {
    // First, so an install while this starts up still restarts it.
    let binary = supervise::reload::binary_at_start();
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
    let token = supervise::reload::ExecToken::inherited().filter(|_| !a.dry_run);
    let Ready {
        dir,
        agents,
        mails,
        mut progress,
        definitions,
        store,
    } = match ready(root, actor, a) {
        Ok(r) => r,
        Err(e) => {
            if let Some(token) = &token {
                finish_handed_off(root, token, &[]);
            }
            return failed(e, a.json);
        }
    };
    if let Some(token) = &token {
        let selected: Vec<String> = agents.iter().map(|s| s.name.clone()).collect();
        finish_handed_off(root, token, &selected);
    }
    let _process_build = if a.dry_run {
        None
    } else {
        crate::version::hold_process(
            root,
            mdkg_run::builds::Kind::Launcher,
            agents.iter().map(|a| a.name.clone()).collect(),
        )
    };
    let supervisor = if a.dry_run {
        None
    } else {
        register_supervisor(&dir, &agents, token.is_some())
    };
    // Explicit names with --once are a manual pass. Default discovery still
    // uses the automatic policy, guard and interactive receiver paths.
    if a.once && !a.agents.is_empty() {
        let mut exit = 0;
        for (index, (agent, mut mail)) in agents.iter().zip(mails).enumerate() {
            // Explicit --once is one manual pass even without a supplied
            // budget. A crash after completion but before finish must not
            // repeat a model turn or dispatch newer mail in this same pass.
            if !a.dry_run && progress[index].record().is_some_and(|r| r.progress.ran > 0) {
                continue;
            }
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
                let result = receiver::supervise_durable(
                    &mut mail,
                    &dir,
                    agent,
                    &how,
                    &|name, event| emit_event(name, &event, a.json),
                    &mut progress[index],
                );
                if let Err(e) = result {
                    return failed(graph_error(e), a.json);
                }
                continue;
            }
            let mut fatal = false;
            // The capacity count carries across passes, as across a watch.
            let mut capacity = supervise::Capacity::load(&dir, &agent.name);
            let capacity_fresh = !a.dry_run && capacity.fresh_next();
            let mut opts = agent.opts.clone();
            opts.flags.fresh |= capacity_fresh;
            let result = (|| {
                let mut saved = match state::load(&dir, &agent.name)? {
                    Some(saved) => saved,
                    None if a.dry_run => {
                        state::resolve(&agent.name, None, opts.flags.clone(), root)?
                    }
                    None => {
                        return Err(mdkg_run::RunError::usage(
                            "run.unknown_agent",
                            "no saved agent",
                        ));
                    }
                };
                // Readers count a turn as running only while its
                // supervisor's record says so (A7).
                if let Some(s) = &supervisor {
                    let _ = s.set(&agent.name, supervise::registry::AgentState::Running);
                }
                let report = if a.dry_run {
                    mdkg_run::turn::turn(&mut mail, &dir, &mut saved, &opts)?
                } else {
                    let watch = supervise::TurnWatch::new(&mut progress[index], None)
                        .inspect_err(|_| fatal = true)?;
                    match mdkg_run::turn::turn_watched(&mut mail, &dir, &mut saved, &opts, watch)
                        .inspect_err(|_| fatal = true)?
                    {
                        supervise::WatchedOutcome::Unclaimed(error) => return Err(error),
                        supervise::WatchedOutcome::Completed { result, .. } => (*result)?,
                    }
                };
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
            if let Some(s) = &supervisor {
                let _ = s.set(&agent.name, supervise::registry::AgentState::Idle);
            }
            if fatal {
                return failed(graph_error(result.expect_err("fatal turn bridge")), a.json);
            }
            if !a.dry_run {
                capacity.after(&result, capacity_fresh);
                if let Err(e) = capacity.save(&dir, &agent.name) {
                    eprintln!("warning: {}", mdkg_run::digest::terminal_safe(&e.message));
                }
            }
            if let Err(error) = &result {
                exit = graph_error(error.clone()).kind.exit_code();
            }
            emit_event(&agent.name, &Event::Turn(&result), a.json);
        }
        if !a.dry_run {
            for saved in &mut progress {
                if let Err(e) = saved.finish() {
                    return failed(graph_error(e), a.json);
                }
            }
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
    let control = supervise::Control::default();
    if let Some(supervisor) = &supervisor {
        control.set_supervisor(supervisor.clone());
    }
    // A long-running watch restarts when a definition it runs on changes,
    // its binary is replaced or a newer build moves the store, and stops on
    // SIGTERM: workers finish their turns first (below). A second SIGTERM
    // takes the default action, at once.
    let term = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if !a.once {
        use signal_hook::{consts::SIGTERM, flag};
        if let Err(e) = flag::register_conditional_default(SIGTERM, term.clone())
            .and_then(|_| flag::register(SIGTERM, term.clone()))
        {
            eprintln!("warning: SIGTERM won't wait for turns to finish: {e}");
        }
    }
    let watch = (!a.once).then(|| supervise::reload::Watch::new(definitions, binary, store));
    let stop = Mutex::new(None::<Stop>);
    let result = std::thread::scope(|s| {
        let watcher = watch.as_ref().map(|watch| {
            s.spawn(|| {
                let mut ticks = 0;
                while !control.stopped() {
                    std::thread::sleep(RELOAD_TICK);
                    let why = if term.load(std::sync::atomic::Ordering::SeqCst) {
                        Some(Stop::Term)
                    } else {
                        ticks += 1;
                        (ticks % RELOAD_POLL_TICKS == 0)
                            .then(|| watch.changed().map(Stop::Restart))
                            .flatten()
                    };
                    if let Some(why) = why {
                        if !json {
                            eprintln!("agents up: {why}; finishing turns first");
                        }
                        *stop.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
                        control.request_stop();
                    }
                }
            })
        });
        let result =
            supervise::up_durable(&make, &dir, &agents, &how, &on, &control, &mut progress);
        control.request_stop();
        if let Some(watcher) = watcher {
            let _ = watcher.join();
        }
        result
    });
    // A SIGTERM during a restart's drain wins: the watcher stopped polling
    // once it chose the restart, so the flag is read again here, and the
    // watch stops instead of re-execing.
    let stop = if term.load(std::sync::atomic::Ordering::SeqCst) {
        Some(Stop::Term)
    } else {
        stop.into_inner().unwrap_or_else(|e| e.into_inner())
    };
    let finish = |progress: &mut Vec<supervise::progress::Locked>| {
        for saved in progress {
            saved.finish().map_err(graph_error)?;
        }
        Ok::<(), GraphError>(())
    };
    // Every agent drained by `agents stop`: done, whatever else was asked.
    let all_stopped = supervisor
        .as_ref()
        .is_some_and(|s| s.stopped().len() == agents.len());
    match result {
        Ok(_) if all_stopped => {
            if let Err(e) = finish(&mut progress) {
                return failed(e, json);
            }
            if !json {
                eprintln!("agents up: every agent stopped by agents stop");
            }
            0
        }
        Ok(_) if stop == Some(Stop::Term) => {
            if let Err(e) = finish(&mut progress) {
                return failed(e, json);
            }
            eprintln!("agents up: stopped by SIGTERM; turns finished");
            0
        }
        Ok(_) if let Some(Stop::Restart(why)) = &stop => {
            // A store that moved under this same binary fails again in a
            // re-exec: stop once, with what to do.
            if let supervise::reload::Reason::Store(_) = why
                && !watch.as_ref().is_some_and(|w| w.binary_replaced())
            {
                if let Err(e) = finish(&mut progress) {
                    return failed(e, json);
                }
                return failed(
                    graph_error(
                        mdkg_run::RunError::failure("coord.store_failed", why.to_string())
                            .with_hint(
                                "a newer mdkg upgraded the store; start agents up with that build",
                            ),
                    ),
                    json,
                );
            }
            // Unfinished on purpose: the re-exec continues each watch.
            let token = supervise::reload::ExecToken {
                pid: std::process::id(),
                watches: progress
                    .iter()
                    .filter(|saved| !saved.released())
                    .filter_map(|saved| saved.record())
                    .map(|r| (r.agent.clone(), r.watch_id.clone()))
                    .collect(),
            };
            eprintln!(
                "agents up: {}; turns finished, restarting",
                mdkg_run::digest::terminal_safe(&why.to_string())
            );
            // The progress locks close on exec, as every descriptor std
            // opens does; the build record goes first, since its Drop won't run.
            drop(_process_build);
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let error = match supervise::reload::invocation_path() {
                Ok(program) => {
                    // The re-exec keeps this supervisor's id, and leaves out
                    // the agents it stopped.
                    let env: Vec<(&str, String)> = supervisor
                        .iter()
                        .map(|s| {
                            (
                                supervise::registry::ENV,
                                supervise::registry::handoff(&s.id(), &s.stopped()),
                            )
                        })
                        .collect();
                    supervise::reload::exec(&program, &token, &env)
                }
                Err(e) => e,
            };
            // One clear stop, never a retry: a broken install fails the same
            // way again. Finished, so starting it by hand needs no resume.
            if let Err(e) = finish(&mut progress) {
                return failed(e, json);
            }
            failed(
                graph_error(mdkg_run::RunError::io(
                    "can't restart agents up; start it again",
                    error,
                )),
                json,
            )
        }
        Ok(ran) => {
            for saved in &mut progress {
                if let Err(e) = saved.finish() {
                    return failed(graph_error(e), json);
                }
            }
            if !json {
                let total: u64 = ran.iter().sum();
                println!("{total} turn(s)");
            }
            0
        }
        Err(e) => failed(graph_error(e), json),
    }
}

/// Names this supervisor (A7), under the id a re-exec inherited, so
/// `agents stop` and `agents status` can find it; and removes stop
/// requests its agents' earlier supervisors left behind when they died
/// (A18). It holds every agent's progress lock by now, so no other
/// supervisor runs them. Failing to register is a warning: the agents
/// run, and `agents stop` can't reach them.
fn register_supervisor(
    dir: &Path,
    agents: &[Supervised],
    re_exec: bool,
) -> Option<std::sync::Arc<supervise::registry::Registered>> {
    let names: Vec<String> = agents.iter().map(|a| a.name.clone()).collect();
    let inherited = re_exec
        .then(supervise::registry::inherited)
        .flatten()
        .map(|(id, _)| id);
    let supervisor = match supervise::registry::Registered::register(
        dir,
        &names,
        Some(crate::version::build()),
        inherited.as_deref(),
    ) {
        Ok(supervisor) => supervisor?,
        Err(e) => {
            eprintln!(
                "warning: can't register this supervisor, so agents stop can't reach it: {}",
                output::shown_path(&e.message)
            );
            return None;
        }
    };
    let id = supervisor.id();
    for name in &names {
        match supervise::stop::read(dir, name) {
            Ok(Some(request)) if request.supervisor_id != id => {
                if supervise::stop::remove(dir, name).is_ok() {
                    eprintln!(
                        "agents up: removed {}'s stop request for supervisor {}, which is no longer running",
                        mdkg_run::digest::terminal_safe(name),
                        mdkg_run::digest::terminal_safe(&request.supervisor_id)
                    );
                }
            }
            Ok(_) => (),
            Err(e) => eprintln!(
                "warning: {}'s stop request: {}",
                mdkg_run::digest::terminal_safe(name),
                output::shown_path(&e.message)
            ),
        }
    }
    Some(std::sync::Arc::new(supervisor))
}

/// A re-exec finishes each watch it was handed but won't continue, as a
/// clean stop would: one it couldn't start (a committed definition it
/// refuses) or one the reloaded config no longer selects. Its predecessor
/// drained it, so nothing is in flight, and starting it again then needs no
/// `agents resume`. `keep` are the watches this process continues. One
/// that's busy, unresolved or not the one handed over is left as it is.
fn finish_handed_off(root: &Path, token: &supervise::reload::ExecToken, keep: &[String]) {
    let Ok(repo) = Repo::open(root) else { return };
    let (Ok(graph), Ok(dir)) = (graph_id(&repo), state::run_dir(&repo.top)) else {
        return;
    };
    for (agent, watch) in &token.watches {
        if keep.contains(agent) {
            continue;
        }
        let Ok(mut saved) = supervise::progress::Locked::open(&dir, graph, agent) else {
            continue;
        };
        if saved
            .record()
            .is_some_and(|r| !r.finished && !r.resume_pending && r.watch_id == *watch)
            && let Err(e) = saved.finish()
        {
            eprintln!(
                "warning: {agent}'s watch is left unfinished: {}",
                mdkg_run::digest::terminal_safe(&e.message)
            );
        }
    }
}

/// How often a long-running watch checks for SIGTERM, and every how many of
/// those ticks its definitions, binary and store.
const RELOAD_TICK: Duration = Duration::from_millis(250);
const RELOAD_POLL_TICKS: u32 = 8;

/// Why a long-running watch drained.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stop {
    Term,
    Restart(supervise::reload::Reason),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Term => f.write_str("SIGTERM"),
            Self::Restart(why) => why.fmt(f),
        }
    }
}

/// `up` refused: with `--json`, one envelope on one line, as its events.
fn failed(mut e: GraphError, json: bool) -> u8 {
    if json {
        print_line_failure(*e.diagnostic);
    } else {
        redact_diagnostic(&mut e.diagnostic);
        output::eprint_diagnostic(&e.diagnostic);
    }
    e.kind.exit_code()
}

/// A failed `agents up` envelope on one line, as its events are.
pub fn print_line_failure(mut d: mdkg_format::Diagnostic) {
    redact_diagnostic(&mut d);
    let envelope: Envelope<UpEvent> = Envelope {
        schema: schema_id("agents.up"),
        ok: false,
        data: None,
        diagnostics: vec![DiagnosticOut::from(&d)],
    };
    println!("{}", serde_json::to_string(&envelope).unwrap_or_default());
}

/// Errors can quote settings during loading, preparation or a later turn.
/// Keep this at the output boundary; runtime values and error codes are intact.
fn redact_diagnostic(diagnostic: &mut Diagnostic) {
    let message = crate::config_cmd::display_text(&diagnostic.message);
    let hidden = message != diagnostic.message;
    diagnostic.message = if hidden {
        "agent configuration or execution failed; credential-like details [redacted]".into()
    } else {
        message
    };
    diagnostic.path = diagnostic
        .path
        .take()
        .map(|s| crate::config_cmd::display_text(&s));
    diagnostic.hint = diagnostic
        .hint
        .take()
        .map(|s| crate::config_cmd::display_text(&s));
    if hidden {
        let repair = format!(
            "inspect `{BIN} config show --effective` for settings and their origins; correct the source setting or command flag, then retry"
        );
        diagnostic.hint = Some(match diagnostic.hint.take() {
            Some(hint) => format!("{repair}; {hint}"),
            None => repair,
        });
    }
}

fn display_error(mut error: GraphError) -> GraphError {
    redact_diagnostic(&mut error.diagnostic);
    error
}

fn receipt_warning(
    agent: &str,
    receipt: &mdkg_run::turn::receipt::delivery::DeliveryStatus,
) -> Option<Diagnostic> {
    receipt.error.as_ref().map(|error| {
        Diagnostic::new(
            Code::RunMailFailed,
            Severity::Warning,
            format!("{agent}: receipt: {error}"),
        )
        .with_hint(match &receipt.path {
            Some(path) => format!(
                "receipt bytes retained at {}; the agent will not be rerun",
                path.display()
            ),
            None => "no local receipt bytes were retained; the agent will not be rerun".into(),
        })
    })
}

fn emit_event(agent: &str, ev: &Event<'_>, json: bool) {
    let safe = mdkg_run::digest::terminal_safe;
    let (data, diag): (UpEvent, Option<GraphError>) = match ev {
        Event::Turn(Ok(r)) => {
            let mut report = r.clone();
            for dropped in &mut report.dropped_allow {
                dropped.rule = crate::config_cmd::display_text(&dropped.rule);
                dropped.fix = crate::config_cmd::display_text(&dropped.fix);
            }
            if let Some(receipt) = &mut report.receipt {
                receipt.error = receipt
                    .error
                    .take()
                    .map(|e| crate::config_cmd::display_text(&e));
            }
            (
                UpEvent {
                    agent: agent.into(),
                    event: "turn".into(),
                    turn: Some(report),
                    wake: None,
                    paused: Vec::new(),
                    cooldown_secs: None,
                },
                None,
            )
        }
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
        Event::Stopped => (
            UpEvent {
                agent: agent.into(),
                event: "stopped".into(),
                turn: None,
                wake: None,
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
    let diag = diag.map(display_error);
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
    let receipt_warning = data
        .turn
        .as_ref()
        .and_then(|turn| turn.receipt.as_ref())
        .and_then(|receipt| receipt_warning(agent, receipt))
        .map(|mut warning| {
            redact_diagnostic(&mut warning);
            warning
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
                .chain(receipt_warning.iter().map(DiagnosticOut::from))
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
        (None, None) if data.event == "stopped" => {
            println!("{}: stopped by agents stop; no more turns", safe(agent));
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
            "{}: loop guard paused {} thread(s) with mail waiting{}; `{BIN} agents resume {} --thread <thread>` lets one run (copy the thread from agents status); stop agents up before full recovery",
            safe(agent),
            data.paused.len(),
            data.cooldown_secs
                .map(|s| format!(", the first runs again in {s} s"))
                .unwrap_or_default(),
            safe(agent)
        ),
    }
}

fn status_report(root: &Path) -> Result<Report<StatusData>, GraphError> {
    let repo = Repo::open(root)?;
    // Ref mode: each worker's lag behind mainRef (G15). Settings that
    // can't be read leave it out; status still reports the rest.
    let main_ref = graph_ref(&repo)
        .ok()
        .flatten()
        .and_then(|g| g.main_ref_base);
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let live = mdkg_run::builds::live(&dir).unwrap_or_else(|e| mdkg_run::builds::Live {
        problems: vec![e.message],
        ..Default::default()
    });
    let build_warnings = live.warnings(&crate::version::build());
    let snapshot = config(&repo)?;
    let cfg = &snapshot.effective.values;
    let conf = configured(cfg);
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
    // The stream head first: a change after it is in the stream, and one
    // before it is in what's read below.
    let event_cursor = mdkg_coord_sqlite::git::store_path(&repo.top)
        .ok()
        .filter(|p| p.exists())
        .and_then(|p| mdkg_coord_sqlite::events::head(&p).ok());
    let (supervisors, _) = supervise::registry::list(&dir).unwrap_or_default();
    for f in supervisors.iter().filter(|f| f.live) {
        for agent in f.record.agents.keys() {
            if !names.contains(agent) {
                names.push(agent.clone());
            }
        }
    }
    let mut agents = Vec::new();
    for name in names {
        let st = state::load(&dir, &name).ok().flatten();
        let effective = crate::config_cmd::effective_agent(&snapshot, &name);
        let (policy, problem) = match Policy::for_agent(cfg, &name) {
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
        let dropped_allow = dropped_allow
            .into_iter()
            .map(|mut dropped| {
                dropped.rule = crate::config_cmd::display_text(&dropped.rule);
                dropped.fix = crate::config_cmd::display_text(&dropped.fix);
                dropped
            })
            .collect();
        let (receipt, receipt_problem) =
            match mdkg_run::turn::receipt::delivery::latest(&dir, &name) {
                Ok(receipt) => (
                    receipt.map(|mut receipt| {
                        receipt.error = receipt.error.map(|e| crate::config_cmd::display_text(&e));
                        receipt
                    }),
                    None,
                ),
                Err(e) => (None, Some(e.message)),
            };
        let billing = harness
            .and_then(|h| mdkg_run::auth::billing_note(&dir, &name, h, &policy.auth))
            .map(|b| crate::config_cmd::display_text(&b));
        let view = mdkg_run::turn::observe::agent(&dir, &name, &supervisors, !paused.is_empty());
        let code_base = main_ref.as_deref().and_then(|main| {
            let st = st
                .as_ref()
                .filter(|s| s.spec.role == mdkg_run::Role::Worker)?;
            let pin = st.spec.pinned.as_ref()?;
            let (main_ref, ahead, behind) = mdkg_run::turn::follow::lag(pin, main)?;
            Some(CodeBase {
                main_ref,
                ahead,
                behind,
            })
        });
        agents.push(AgentStatus {
            code_base,
            state: view.state,
            supervisor: view.supervisor,
            current_turn: view.current_turn,
            last_turn: view.last_turn,
            follow_from: view.follow_from,
            effective,
            auth,
            billing,
            configured: conf.contains(&name),
            known: st.is_some(),
            harness: st.as_ref().map(|s| s.spec.harness),
            role: st.as_ref().map(|s| s.spec.role),
            cwd: st.as_ref().map(|s| s.spec.cwd.clone()),
            turns: st.as_ref().map_or(0, |s| s.turns),
            name,
            policy,
            paused,
            problem: problem.map(|p| crate::config_cmd::display_text(&p)),
            receipt,
            receipt_problem: receipt_problem.map(|p| crate::config_cmd::display_text(&p)),
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
    // An unreadable progress inventory must remain visible without hiding
    // unrelated threads, receivers, and build diagnostics. This is display
    // only; startup and recovery still propagate the same refusal.
    let (progress, diagnostics) = match progress_status(root) {
        Ok(progress) => (progress, Vec::new()),
        Err(error) => {
            let mut diagnostic = *error.diagnostic;
            diagnostic.severity = Severity::Warning;
            (Vec::new(), vec![diagnostic])
        }
    };
    Ok(Report::ok(StatusData {
        event_cursor,
        agents,
        config_warnings: snapshot
            .warnings
            .iter()
            .map(|w| crate::config_cmd::display_text(&w.to_string()))
            .collect(),
        threads,
        receivers,
        processes: live.processes,
        build_warnings,
        progress,
    })
    .with(diagnostics))
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

/// Read-only snapshots; never open the writer or create a store for diagnostics.
pub(crate) fn progress_status(root: &Path) -> Result<Vec<ProgressStatus>, GraphError> {
    let repo = Repo::open(root)?;
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let names = supervise::progress::actors(&dir).map_err(graph_error)?;
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let graph = graph_id(&repo)?;
    names.into_iter().map(|name| {
        let mut out = ProgressStatus { agent: name.clone(), active: false, record: None, current_store: None, archive: None, problem: None, repair: None };
        match supervise::progress::inspect_status(&dir, graph, &name) {
            Ok(Some(snapshot)) => {
                out.active = snapshot.active;
                let view = snapshot.recovery;
                out.current_store = Some(view.current_store_id());
                out.record = Some(serde_json::to_value(&view.record).map_err(|e| bad_agent(&name, e))?);
                out.archive = view.context_archive.clone();
                let error = view.require_current_store().err()
                    .or(view.archive_problem)
                    .or(snapshot.lock_problem)
                    .or_else(|| (!out.active).then(|| view.record.require_ready().err()).flatten());
                if let Some(error) = error { out.problem = Some(error.message); out.repair = error.hint; }
            }
            Ok(None) => (),
            Err(error) => { out.problem = Some(error.message); out.repair = error.hint.or_else(|| Some("preserve the progress file and restore a verified copy; do not reset its budget".into())); }
        }
        Ok(out)
    }).collect()
}

fn resume_report(
    root: &Path,
    agent: &str,
    thread: Option<&str>,
    store: Option<&str>,
    archive_context: bool,
) -> Result<ResumeData, GraphError> {
    state::check_name(agent).map_err(graph_error)?;
    let repo = Repo::open(root)?;
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
        .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
    let graph = graph_id(&repo)?;
    let view = supervise::progress::inspect_recovery(&dir, graph, agent).map_err(graph_error)?;
    if thread.is_some() && (store.is_some() || archive_context) {
        return Err(bad_agent(agent, "--thread cannot recover durable progress"));
    }
    if let Some(view) = &view {
        if let Some(error) = &view.archive_problem {
            return Err(graph_error(error.clone()));
        }
        if store.is_some_and(|id| id != view.current_store_id()) {
            return Err(bad_agent(
                agent,
                format!(
                    "store confirmation does not match current {}",
                    view.current_store_id()
                ),
            ));
        }
        if store.is_none() {
            view.require_current_store().map_err(graph_error)?;
        }
        if thread.is_some() {
            view.record.require_ready().map_err(graph_error)?;
        }
    } else if store.is_some() || archive_context {
        return Err(bad_agent(agent, "no supervisor progress to recover"));
    }
    let mut saved = if thread.is_none() && view.is_some() {
        Some(
            supervise::progress::Locked::open_for_resume(&dir, graph, agent)
                .map_err(graph_error)?,
        )
    } else {
        None
    };
    // Locking re-reads and validates the record. Show that exact snapshot
    // before any recovery, guard reset, or receiver route mutation.
    let progress_before = saved
        .as_ref()
        .and_then(|s| s.record())
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| bad_agent(agent, e))?;
    if let Some(record) = &progress_before {
        eprintln!(
            "supervisor recovery metadata: {}",
            mdkg_run::digest::terminal_safe(&record.to_string())
        );
    }
    let mut recovery_archive = None;
    let interactive = receiver::mode(&repo.root, &dir, agent).map_err(graph_error)?
        == receiver::Mode::Interactive;
    if view.is_none()
        && state::load(&dir, agent).map_err(graph_error)?.is_none()
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
    if let Some(saved) = &mut saved {
        let recovered = if archive_context {
            saved.archive_context(store)
        } else {
            saved.resume(store)
        }
        .map_err(graph_error)?;
        recovery_archive = recovered.archive;
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
        progress_before,
        recovery_archive,
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
        AgentsCommand::Logs(a) => logs::run(root, a),
        AgentsCommand::Stop(a) => {
            let r = stop::stop(root, actor, a).unwrap_or_else(Report::failed);
            output::emit(c.name(), a.json, r, |d| {
                for agent in &d.agents {
                    println!("{}: {}", safe(&agent.name), agent.state.replace('_', " "));
                }
            })
        }
        AgentsCommand::UseKey { agent, json } => {
            let r = use_key(root, actor, agent).map_or_else(Report::failed, Report::ok);
            output::emit(c.name(), *json, r, |d| {
                println!(
                    "{}: the next turn bills your API key ({})",
                    safe(&d.agent),
                    safe(&d.mark.to_string_lossy())
                );
                if let Some(p) = &d.note_problem {
                    println!("  warning: no note was sent to wake the agent: {}", safe(p));
                }
            })
        }
        AgentsCommand::Status { json } => {
            // D9's output-boundary redaction applies to a failed report, to D6's
            // progress warnings and to D2's receipt warnings alike.
            let r = match status_report(root).map_err(display_error) {
                Err(e) => Report::failed(e),
                Ok(mut report) => {
                    // D6's progress problems arrive as warnings inside the report.
                    for diagnostic in &mut report.diagnostics {
                        redact_diagnostic(diagnostic);
                    }
                    let diagnostics: Vec<_> = report
                        .data
                        .iter()
                        .flat_map(|data| data.agents.iter())
                        .filter_map(|agent| {
                            agent.receipt_problem.as_ref().map_or_else(
                                || {
                                    agent
                                        .receipt
                                        .as_ref()
                                        .and_then(|r| receipt_warning(&agent.name, r))
                                },
                                |error| {
                                    Some(Diagnostic::new(
                                        Code::RunMailFailed,
                                        Severity::Warning,
                                        format!("{}: {error}", agent.name),
                                    ))
                                },
                            )
                        })
                        .map(|mut diagnostic| {
                            redact_diagnostic(&mut diagnostic);
                            diagnostic
                        })
                        .collect();
                    report.with(diagnostics)
                }
            };
            output::emit("agents.status", *json, r, |d| {
                for progress in &d.progress {
                    // Its record's fields, nested by indent (Opus final on
                    // a6ba7012).
                    println!("progress {}:", safe(&progress.agent));
                    if let Some(record) = &progress.record {
                        for line in crate::entry::field_lines("    ", record) {
                            println!("{line}");
                        }
                    }
                    if progress.active {
                        println!("  watch active");
                    }
                    if let Some(problem) = &progress.problem {
                        println!("  {}", safe(problem));
                    }
                    if let Some(repair) = &progress.repair {
                        println!("  {}", safe(repair));
                    }
                    if let Some(archive) = &progress.archive {
                        println!(
                            "  recovery archive: {}",
                            safe(&archive.display().to_string())
                        );
                    }
                }
                for warning in &d.build_warnings {
                    println!("warning: {}", safe(warning));
                }
                for warning in &d.config_warnings {
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
                        "{}: {}, {what}, {} turn(s), trigger {}{}",
                        safe(&a.name),
                        a.state,
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
                    if let Some(c) = &a.code_base {
                        println!(
                            "  code base: {} ahead, {} behind {}",
                            c.ahead,
                            c.behind,
                            safe(&c.main_ref)
                        );
                    }
                    if let Some(effective) = &a.effective {
                        // One field a line, nested by indent, so each value
                        // reads under its own field (Opus final on a6ba7012).
                        println!("  effective:");
                        let value = serde_json::to_value(effective).expect("effective agent");
                        for line in crate::entry::field_lines("    ", &value) {
                            println!("{line}");
                        }
                    }
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
                    if let Some(b) = &a.billing {
                        println!("  billing: {}", safe(b));
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
            store,
            archive_context,
            json,
        } => {
            let r = resume_report(
                root,
                agent,
                thread.as_deref(),
                store.as_deref(),
                *archive_context,
            )
            .map_or_else(Report::failed, Report::ok);
            output::emit("agents.resume", *json, r, |d| {
                println!(
                    "{}: {} thread(s) let run again",
                    safe(&d.agent),
                    d.resumed.len()
                );
                if let Some(archive) = &d.recovery_archive {
                    println!("recovery archive: {}", safe(&archive.display().to_string()));
                }
                if d.waiting_for_prompt {
                    println!("waiting for the next primary Codex prompt; no route selected");
                }
            })
        }
    }
}
