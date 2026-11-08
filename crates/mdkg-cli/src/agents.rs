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
use mdkg_run::supervise::registry::ExitReason;
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
mod turns;
pub use logs::{LogsArgs, LogsData};
pub use stop::{StopArgs, StopData};
pub use turns::{TurnsArgs, TurnsData};

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
    /// List an agent's turns, newest first: each one's id, number, times,
    /// outcome and receipt, a page at a time.
    #[command(after_help = examples!("agents turns worker-1 --json", "agents turns worker-1 --limit 20 --before 41 --json", "agents turns worker-1 --turn 7 --json"))]
    Turns(TurnsArgs),
    /// Show the agents, their policies, and the threads the loop guard
    /// paused.
    #[command(after_help = examples!("agents status", "agents status --json", "agents status --no-probe --json"))]
    Status {
        /// Run no probe: no harness login check, Keychain listing or
        /// billing read, so it's cheap to poll. `auth` and `billing` are
        /// left out.
        #[arg(long)]
        no_probe: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the agents the settings define, each with the layer that
    /// defines it and every layer with an entry for it. Reads settings only:
    /// nothing is probed and no saved state is read.
    #[command(after_help = examples!("agents list", "agents list --json"))]
    List {
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
            AgentsCommand::Turns(_) => "agents.turns",
            AgentsCommand::Status { .. } => "agents.status",
            AgentsCommand::List { .. } => "agents.list",
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
            AgentsCommand::Turns(a) => a.json,
            AgentsCommand::Status { json, .. }
            | AgentsCommand::List { json }
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
    /// Mail delivery shape (batch by default).
    #[arg(long, value_parser = ["batch", "digest"])]
    pub delivery: Option<String>,
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
                branch: None,
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
            if let Some(delivery) = &a.delivery {
                let policy = object
                    .entry("policy")
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .ok_or_else(|| bad_agent(&a.agent, "policy must be an object"))?;
                policy.insert("shape".into(), Value::String(delivery.clone()));
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
    /// Defined by its own file, `.mdkg/agents/<name>.json` (then
    /// `effective` is its layered definition), rather than inline in the
    /// config.
    pub file_backed: bool,
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
    /// Which environment its turns start with: `allowlist`, `inherit`
    /// (`env.inherit: true`, the launcher's whole environment less provider
    /// keys), or `session` for an interactive receiver. Names only, never
    /// values (crate::env in mdkg-run).
    pub env_mode: EnvMode,
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
    /// check and the Keychain's list (no key is read). Absent under
    /// `--no-probe`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<mdkg_run::auth::AuthStatus>,
    /// The key, offered or marked: after a usage limit, the `agents use-key`
    /// command to bill the next turn on it; a mark waiting for that turn;
    /// or a mark held because it couldn't be recorded durably. Absent
    /// under `--no-probe`.
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
    /// How its last supervisor ended, while none runs it: `reason` is
    /// `cap` (`--max-turns` reached), `once`, `manual` (a `manual`
    /// trigger: a watch runs it no turns), `mode_changed` (its receiver
    /// changed mode), `drained` (`agents stop`, as of the drain), `signal`
    /// (SIGTERM, turns finished) or `failed` (with `code`). Only for the
    /// agent's latest watch, once it's finished.
    /// Absent while a supervisor runs it, and when none recorded an exit
    /// (never run, or killed outright: SIGKILL, SIGINT, a crash), so a
    /// reason is never guessed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<LastExit>,
    /// The turn lease a turn couldn't give back as it ended (its release
    /// failed, as a busy store's `coord.store_failed` does): it may still
    /// be held, so the next turn's start releases it first. Absent once a
    /// turn holds the lease again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_lease_unreleased: Option<TurnLeaseUnreleased>,
    /// The position `agents logs <agent> --follow --after` takes to pick up
    /// from this snapshot: never "from now", so a turn that starts or ends
    /// in between is still delivered.
    pub follow_from: String,
    /// A worker's branch against mainRef (ref mode, G15): absent when
    /// there's no mainRef here, or no worktree yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_base: Option<CodeBase>,
}

/// How an agent's last supervisor ended (`agents status`'s `last_exit`).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LastExit {
    pub supervisor_id: String,
    pub reason: supervise::registry::ExitReason,
    /// The failure's diagnostic code, with `failed` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// When it exited (RFC 3339).
    pub at: String,
}

/// A turn lease release that failed (`agents status`'s
/// `turn_lease_unreleased`).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TurnLeaseUnreleased {
    /// The failed release's diagnostic code.
    pub code: String,
    pub message: String,
    /// When the release failed (RFC 3339).
    pub at: String,
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

/// One warning per agent whose turns inherit the launcher's whole
/// environment: `env.inherit: true`, or run by a live supervisor from a
/// build before 0.7.2 (`untracked`). By name.
fn inherit_warnings(agents: &[AgentStatus], untracked: &[String]) -> Vec<String> {
    let mut warnings: Vec<String> = agents
        .iter()
        .filter(|a| a.env_mode == EnvMode::Inherit)
        .map(|a| {
            format!(
                "agent {} has env.inherit: its turns get the launcher's whole environment, so every credential the launcher's shell exports reaches it; to apply a change, publish it where agents read their config (in checkout mode, run `{BIN} agents edit` in the graph checkout or with `--root` naming it; in ref mode, from any checkout), restart `agents up` (a one-shot `--once` pass keeps the policies it read at start until it ends), then check each agent's `env_mode` and `policy.env.allow` in `agents status --json`, which reads the config from the same place",
                a.name
            )
        })
        .collect();
    warnings.extend(untracked.iter().map(|name| {
        format!(
            "agent {name} runs in a supervisor not confirmed as 0.7.2 or later (an older build, or one that couldn't register), whose turns may get the launcher's whole environment; restart `agents up` with this build"
        )
    }));
    warnings
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

/// `agents list`: the agents the settings define.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListData {
    pub agents: Vec<crate::config_cmd::ListedAgent>,
    pub config_warnings: Vec<String>,
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

/// The environment an agent's turns start with, as `agents status` and
/// `doctor` report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnvMode {
    /// Only the allowlist and the policy's `env.allow`.
    Allowlist,
    /// The launcher's whole environment, less provider keys (`env.inherit`).
    Inherit,
    /// An interactive receiver: mdkg delivers into a session someone
    /// started, which has that person's environment.
    Session,
}

impl EnvMode {
    fn of(policy: mdkg_run::env::Mode, interactive: bool) -> Self {
        match (interactive, policy) {
            (true, _) => Self::Session,
            (false, mdkg_run::env::Mode::Allowlist) => Self::Allowlist,
            (false, mdkg_run::env::Mode::Inherit) => Self::Inherit,
        }
    }
}

/// An agent's name and its environment mode, or why it can't be read.
pub(crate) type EnvRow = (String, Result<EnvMode, String>);

/// Every agent `agents status` lists, in its order: the config's, then the
/// saved ones, the receivers' and the ones a live supervisor runs.
fn agent_names(
    cfg: &Value,
    dir: &Path,
    receiver_common: &Path,
    supervisors: &[supervise::registry::Found],
) -> Vec<String> {
    let mut names = configured(cfg);
    let others = known(dir)
        .into_iter()
        .chain(receiver::route::actors(receiver_common))
        .chain(
            supervisors
                .iter()
                .filter(|f| f.live)
                .flat_map(|f| f.record.agents.keys().cloned()),
        );
    for k in others {
        if !names.contains(&k) {
            names.push(k);
        }
    }
    names
}

/// Each agent's environment mode, by name (`doctor`), for the same agents
/// as `agents status`: its policy's `env`, `session` for an interactive
/// receiver, or why either can't be read. Names only.
pub(crate) fn env_modes(root: &Path) -> Result<Vec<EnvRow>, GraphError> {
    let repo = Repo::open(root)?;
    let snapshot = config(&repo)?;
    let cfg = &snapshot.effective.values;
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
        .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
    let (supervisors, _) = supervise::registry::list(&dir).unwrap_or_default();
    Ok(agent_names(cfg, &dir, &common, &supervisors)
        .into_iter()
        .map(|name| {
            let mode = Policy::for_agent(cfg, &name)
                .map_err(|e| e.message)
                .map(|p| {
                    // As `agents status` does: unreadable receiver
                    // evidence counts as headless, so the policy's mode
                    // is reported rather than a misleading "unreadable".
                    let interactive = receiver::mode(&repo.root, &dir, &name)
                        .is_ok_and(|m| m == receiver::Mode::Interactive);
                    EnvMode::of(p.env.mode(), interactive)
                });
            (name, mode)
        })
        .collect())
}

/// The agents a live supervisor runs that aren't confirmed to start turns
/// with the policy's environment (`doctor`; see
/// [`supervise::registry::unconfirmed_env`]). Names only.
pub(crate) fn unconfirmed_env(root: &Path) -> Vec<String> {
    let Ok(repo) = Repo::open(root) else {
        return Vec::new();
    };
    let Ok(dir) = state::run_dir(&repo.top) else {
        return Vec::new();
    };
    let (found, _) = supervise::registry::list(&dir).unwrap_or_default();
    let processes = mdkg_run::builds::live(&dir)
        .map(|l| l.processes)
        .unwrap_or_default();
    supervise::registry::unconfirmed_env(&found, &processes)
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
    check_selected(&names)?;
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
            opts.env = p.policy.env.clone();
            opts.limit = p.policy.batch_max();
            opts.digest_budget = match &p.policy.shape {
                mdkg_run::policy::Shape::Digest { budget } => Some(*budget),
                _ => None,
            };
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

/// The selected agent names' own checks: each a valid name, none twice.
/// Pure, so `up` runs them before its first graph pull too.
fn check_selected(names: &[String]) -> Result<(), GraphError> {
    for name in names {
        state::check_name(name).map_err(graph_error)?;
    }
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort();
    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(bad_agent("launcher", "duplicate selected agent"));
    }
    Ok(())
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
    // `ready`'s checks of the arguments alone, before the first pull below
    // can touch the network (DECISION 19 (T1)); it checks them again.
    if let Err(e) = check_selected(&a.agents) {
        if let Some(token) = &token {
            finish_handed_off(root, token, &[], None);
        }
        return failed(e, a.json);
    }
    // G13 (answer (b)): graph sync's timers, in ref mode, with or without
    // a remote yet (one configured while `up` runs is picked up, Terra
    // #18984). The first pull runs here, before `ready` reads the
    // definitions and policies turns are prepared from. No network sync
    // starts inside an agent's turn (DECISION 19 (T1)): the syncer starts
    // only after the outside-turn check `graph sync` and `graph remote
    // approve` make; inside one, `up` runs without sync and says so once.
    let sync_graph = (!a.once)
        .then(|| crate::coord::Repo::open(root).ok())
        .flatten()
        .and_then(|repo| {
            mdkg_graph::graphref::resolve(&repo.top)
                .ok()
                .flatten()
                .map(|g| (repo.top, g))
        })
        .filter(|(top, graph)| {
            match crate::graph::sync::outside_turn(top, graph, "graph.sync_in_turn") {
                Ok(()) => true,
                Err(e) => {
                    eprintln!(
                        "agents up: graph sync is off for this run: {}",
                        mdkg_run::digest::terminal_safe(&e.to_string())
                    );
                    false
                }
            }
        });
    let syncer = sync_graph
        .as_ref()
        .map(|(top, graph)| crate::graph::rows::Syncer::start(top, graph, a.json));
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
                finish_handed_off(root, token, &[], Some(e.diagnostic.code.as_str()));
            }
            return failed(e, a.json);
        }
    };
    if let Some(token) = &token {
        let selected: Vec<String> = agents.iter().map(|s| s.name.clone()).collect();
        finish_handed_off(root, token, &selected, None);
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
                    record_exit(
                        supervisor.as_deref(),
                        &progress,
                        ExitReason::Failed,
                        Some(e.code.as_str()),
                    );
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
                let e = result.expect_err("fatal turn bridge");
                record_exit(
                    supervisor.as_deref(),
                    &progress,
                    ExitReason::Failed,
                    Some(e.code.as_str()),
                );
                return failed(graph_error(e), a.json);
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
        if !a.dry_run
            && let Some(e) = progress.iter_mut().find_map(|saved| saved.finish().err())
        {
            record_exit(
                supervisor.as_deref(),
                &progress,
                ExitReason::Failed,
                Some(e.code.as_str()),
            );
            return failed(graph_error(e), a.json);
        }
        // The pass ran; a turn that failed is its own last_turn.
        record_exit(supervisor.as_deref(), &progress, ExitReason::Once, None);
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
    // Registered by main before anything ran (`register_term`), so a
    // SIGTERM during startup's git (a worker's `git worktree add`) waits
    // for it instead of killing mdkg under it.
    let term = term_flag();
    let watch = (!a.once).then(|| supervise::reload::Watch::new(definitions, binary, store));
    let stop = Mutex::new(None::<Stop>);
    let drained = std::sync::atomic::AtomicBool::new(false);
    // The control reads the SIGTERM flag itself: once the handler has run,
    // no worker is admitted to another turn, with no watcher tick between
    // (decision 01a1a7dc). The exit below reads the flag again for
    // Stop::Term.
    if !a.once {
        control.stop_on(term.clone());
    }
    let result = std::thread::scope(|s| {
        let watcher = watch.as_ref().map(|watch| {
            s.spawn(|| {
                let mut ticks = 0;
                loop {
                    // The SIGTERM flag first: control already reads it, so
                    // `stopped()` turns true with it, and the stop it asked
                    // for is named here before the loop ends.
                    let why = if term.load(std::sync::atomic::Ordering::SeqCst) {
                        Some(Stop::Term)
                    } else if control.stopped() {
                        break;
                    } else {
                        std::thread::sleep(RELOAD_TICK);
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
                        break;
                    }
                }
            })
        });
        // G13 (answer (b)): graph sync's pull and push timers, with a graph
        // remote in ref mode only; they push only to an approved, unchanged
        // destination (graph::rows::Syncer). The first pull finishes before
        // any turn is admitted, and the timers run until every turn has
        // drained (Terra #18237).
        let drained = &drained;
        let syncer = sync_graph.as_ref().zip(syncer).map(|((top, _), syncer)| {
            s.spawn(move || {
                syncer.run(
                    top,
                    &|| drained.load(std::sync::atomic::Ordering::SeqCst),
                    json,
                )
            })
        });
        let result =
            supervise::up_durable(&make, &dir, &agents, &how, &on, &control, &mut progress);
        drained.store(true, std::sync::atomic::Ordering::SeqCst);
        control.request_stop();
        if let Some(watcher) = watcher {
            let _ = watcher.join();
        }
        if let Some(syncer) = syncer {
            let _ = syncer.join();
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
    let exited = |progress: &[supervise::progress::Locked], reason, code| {
        record_exit(supervisor.as_deref(), progress, reason, code)
    };
    match result {
        Ok(_) if all_stopped => {
            if let Err(e) = finish(&mut progress) {
                exited(
                    &progress,
                    ExitReason::Failed,
                    Some(e.diagnostic.code.as_str()),
                );
                return failed(e, json);
            }
            exited(&progress, ExitReason::Drained, None);
            if !json {
                eprintln!("agents up: every agent stopped by agents stop");
            }
            0
        }
        Ok(_) if stop == Some(Stop::Term) => {
            if let Err(e) = finish(&mut progress) {
                exited(
                    &progress,
                    ExitReason::Failed,
                    Some(e.diagnostic.code.as_str()),
                );
                return failed(e, json);
            }
            exited(&progress, ExitReason::Signal, None);
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
                    exited(
                        &progress,
                        ExitReason::Failed,
                        Some(e.diagnostic.code.as_str()),
                    );
                    return failed(e, json);
                }
                exited(&progress, ExitReason::Failed, Some("coord.store_failed"));
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
                exited(
                    &progress,
                    ExitReason::Failed,
                    Some(e.diagnostic.code.as_str()),
                );
                return failed(e, json);
            }
            exited(
                &progress,
                ExitReason::Failed,
                Some(mdkg_run::error::CODE_IO),
            );
            failed(
                graph_error(mdkg_run::RunError::io(
                    "can't restart agents up; start it again",
                    error,
                )),
                json,
            )
        }
        Ok(ran) => {
            if let Some(e) = progress.iter_mut().find_map(|saved| saved.finish().err()) {
                exited(&progress, ExitReason::Failed, Some(e.code.as_str()));
                return failed(graph_error(e), json);
            }
            // Each agent's own end, as its worker noted it (#20977). One
            // that noted none ended a --once pass, or isn't recorded: its
            // reason is never guessed. A drained agent was recorded as it
            // was let go, and its released lock writes nothing here.
            for (agent, held) in agents.iter().zip(&progress) {
                let (reason, code) = match control.ended(&agent.name) {
                    Some(supervise::End::Manual) => (ExitReason::Manual, None),
                    Some(supervise::End::Cap) => (ExitReason::Cap, None),
                    Some(supervise::End::ModeChanged) => (ExitReason::ModeChanged, None),
                    Some(supervise::End::UnknownAgent) => (
                        ExitReason::Failed,
                        Some(mdkg_run::error::CODE_UNKNOWN_AGENT),
                    ),
                    None if a.once => (ExitReason::Once, None),
                    None => continue,
                };
                record_exit(
                    supervisor.as_deref(),
                    std::slice::from_ref(held),
                    reason,
                    code,
                );
            }
            if !json {
                let total: u64 = ran.iter().sum();
                println!("{total} turn(s)");
            }
            0
        }
        Err(e) => {
            exited(&progress, ExitReason::Failed, Some(e.code.as_str()));
            failed(graph_error(e), json)
        }
    }
}

/// Records why this `agents up` ended, kept per agent once its process is
/// gone (#20587). A failure to record it is a warning.
fn record_exit(
    supervisor: Option<&supervise::registry::Registered>,
    progress: &[supervise::progress::Locked],
    reason: ExitReason,
    code: Option<&str>,
) {
    let Some(s) = supervisor else { return };
    for held in progress {
        if let Err(e) = supervise::registry::record_exit(s.run_dir(), &s.id(), held, reason, code) {
            exit_warning(&e);
        }
    }
}

fn exit_warning(e: &mdkg_run::RunError) {
    eprintln!(
        "warning: can't record why agents up exited: {}",
        mdkg_run::digest::terminal_safe(&e.message)
    );
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
///
/// `failed` is the code of a re-exec that couldn't start: each watch it
/// finishes here gets a `failed` exit with that code, written under that
/// watch's progress lock and the supervisor id the re-exec inherited
/// (#20977). One that's busy (another process holds it) gets none.
fn finish_handed_off(
    root: &Path,
    token: &supervise::reload::ExecToken,
    keep: &[String],
    failed: Option<&str>,
) {
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
        if !saved
            .record()
            .is_some_and(|r| !r.finished && !r.resume_pending && r.watch_id == *watch)
        {
            continue;
        }
        if let Err(e) = saved.finish() {
            eprintln!(
                "warning: {agent}'s watch is left unfinished: {}",
                mdkg_run::digest::terminal_safe(&e.message)
            );
            continue;
        }
        if let Some(code) = failed
            && let Some((id, _)) = supervise::registry::inherited()
            && let Err(e) =
                supervise::registry::record_exit(&dir, &id, &saved, ExitReason::Failed, Some(code))
        {
            exit_warning(&e);
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

/// `probe`: run each agent's harness login check, list the Keychain and
/// read its billing mark (`auth`, `billing`); without it, nothing but
/// mdkg's own records and git is read.
fn status_report(root: &Path, probe: bool) -> Result<Report<StatusData>, GraphError> {
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
    let receiver_common = mdkg_coord_sqlite::git::common_dir(&repo.top)
        .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
    let now = state::now_ms();
    // The stream head first: a change after it is in the stream, and one
    // before it is in what's read below.
    let event_cursor = mdkg_coord_sqlite::git::store_path(&repo.top)
        .ok()
        .filter(|p| p.exists())
        .and_then(|p| mdkg_coord_sqlite::events::head(&p).ok());
    let (supervisors, _) = supervise::registry::list(&dir).unwrap_or_default();
    // For last_exit's watch check; without it, no exit is shown.
    let graph = graph_id(&repo).ok();
    let names = agent_names(cfg, &dir, &receiver_common, &supervisors);
    let mut agents = Vec::new();
    for name in names {
        let st = state::load(&dir, &name).ok().flatten();
        let effective = crate::config_cmd::effective_agent(&snapshot, &name);
        let file_backed = snapshot.definition_names.contains(&name);
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
        let auth = harness.filter(|_| probe).map(|h| {
            mdkg_run::auth::status(
                h,
                &h.program(),
                &cwd,
                &policy.auth,
                &mdkg_run::auth::KeySource::default(),
                &mdkg_run::env::Environment::new(h, &policy.env),
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
            .filter(|_| probe)
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
        // Shown unless a live supervisor runs it, undrained, and only
        // while its record names the agent's current, finished watch
        // (#20977): a later run hides an older reason, nothing to clear.
        let last_exit = (!supervise::registry::holds(&supervisors, &name))
            .then(|| {
                let progress = graph
                    .and_then(|g| supervise::progress::inspect(&dir, g, &name).ok())
                    .flatten();
                supervise::registry::current_exit(&dir, &name, progress.as_ref())
            })
            .flatten()
            .map(|e| LastExit {
                supervisor_id: e.supervisor_id,
                reason: e.reason,
                code: e.code,
                at: supervise::timestamp(e.at_ms),
            });
        let turn_lease_unreleased =
            mdkg_run::state::read_release_failure(&dir, &name).map(|f| TurnLeaseUnreleased {
                code: crate::config_cmd::display_text(&f.code),
                message: crate::config_cmd::display_text(&f.message),
                at: supervise::timestamp(f.at_ms),
            });
        agents.push(AgentStatus {
            code_base,
            last_exit,
            turn_lease_unreleased,
            state: view.state,
            supervisor: view.supervisor,
            current_turn: view.current_turn,
            last_turn: view.last_turn,
            follow_from: view.follow_from,
            effective,
            auth,
            billing,
            configured: conf.contains(&name),
            file_backed,
            known: st.is_some(),
            harness: st.as_ref().map(|s| s.spec.harness),
            role: st.as_ref().map(|s| s.spec.role),
            cwd: st.as_ref().map(|s| s.spec.cwd.clone()),
            turns: st.as_ref().map_or(0, |s| s.turns),
            env_mode: EnvMode::of(
                policy.env.mode(),
                receiver::mode(&repo.root, &dir, &name)
                    .is_ok_and(|m| m == receiver::Mode::Interactive),
            ),
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
    let mut config_warnings: Vec<String> = snapshot
        .warnings
        .iter()
        .map(|w| crate::config_cmd::display_text(&w.to_string()))
        .collect();
    config_warnings.extend(inherit_warnings(
        &agents,
        &supervise::registry::unconfirmed_env(&supervisors, &live.processes),
    ));
    Ok(Report::ok(StatusData {
        event_cursor,
        agents,
        config_warnings,
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
        AgentsCommand::Turns(a) => {
            let r = turns::report(root, a);
            output::emit(c.name(), a.json, r, |d| {
                for t in &d.turns {
                    println!(
                        "{} {} {} {}",
                        t.number,
                        safe(&t.turn_id),
                        safe(&t.started),
                        safe(t.outcome.as_deref().unwrap_or(&t.state))
                    );
                }
                if let Some(n) = d.next_before {
                    println!("more: --before {n}");
                }
            })
        }
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
        AgentsCommand::List { json } => {
            let r = crate::config_cmd::agents(root)
                .map(|(agents, config_warnings)| {
                    Report::ok(ListData {
                        agents,
                        config_warnings,
                    })
                })
                .map_err(display_error)
                .unwrap_or_else(Report::failed);
            output::emit(c.name(), *json, r, |d| {
                for a in &d.agents {
                    let layers: Vec<String> = a
                        .layers
                        .iter()
                        .map(|l| {
                            serde_json::to_value(l)
                                .ok()
                                .and_then(|v| v.as_str().map(str::to_owned))
                                .unwrap_or_default()
                        })
                        .collect();
                    println!(
                        "{}: defined in {}{}; layers {}{}{}",
                        safe(&a.name),
                        serde_json::to_value(a.defined_in)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default(),
                        if a.file_backed { " (its own file)" } else { "" },
                        layers.join(", "),
                        a.harness
                            .as_deref()
                            .map(|h| format!("; harness {}", safe(h)))
                            .unwrap_or_default(),
                        a.role
                            .as_deref()
                            .map(|r| format!("; role {}", safe(r)))
                            .unwrap_or_default(),
                    );
                }
                for w in &d.config_warnings {
                    eprintln!("warning: {}", safe(w));
                }
            })
        }
        AgentsCommand::Status { json, no_probe } => {
            // D9's output-boundary redaction applies to a failed report, to D6's
            // progress warnings and to D2's receipt warnings alike.
            let r = match status_report(root, !*no_probe).map_err(display_error) {
                Err(e) => Report::failed(e),
                Ok(mut report) => {
                    // D6's progress problems arrive as warnings inside the report.
                    for diagnostic in &mut report.diagnostics {
                        redact_diagnostic(diagnostic);
                    }
                    let unreleased = report
                        .data
                        .iter()
                        .flat_map(|data| data.agents.iter())
                        .filter_map(|agent| {
                            let u = agent.turn_lease_unreleased.as_ref()?;
                            Some(
                                Diagnostic::new(
                                    Code::RunMailFailed,
                                    Severity::Warning,
                                    format!(
                                        "{}: its turn lease wasn't released as a turn ended ({}, at {}): {}",
                                        agent.name, u.code, u.at, u.message
                                    ),
                                )
                                .with_hint(
                                    "the next turn's start releases it first; until then it may block a turn",
                                ),
                            )
                        })
                        .collect::<Vec<_>>();
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
                        .chain(unreleased)
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
                    println!(
                        "  environment: {}",
                        match a.env_mode {
                            EnvMode::Allowlist => "allowlist",
                            EnvMode::Inherit =>
                                "inherit (env.inherit: the launcher's whole environment)",
                            EnvMode::Session => "session (an interactive receiver's own)",
                        }
                    );
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

static TERM: std::sync::OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> =
    std::sync::OnceLock::new();

fn term_flag() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    TERM.get_or_init(Default::default).clone()
}

/// A continuous `agents up` stops on SIGTERM by draining: its turns finish
/// first, and a second SIGTERM takes the default action at once. Called by
/// main before the command does anything, so no git it starts (startup's
/// included) is left running by a SIGTERM's default action.
pub(crate) fn register_term() {
    use signal_hook::{consts::SIGTERM, flag};
    let term = term_flag();
    if let Err(e) = flag::register_conditional_default(SIGTERM, term.clone())
        .and_then(|_| flag::register(SIGTERM, term))
    {
        eprintln!("warning: SIGTERM won't wait for turns to finish: {e}");
    }
}

#[cfg(test)]
mod env_mode_tests {
    use super::EnvMode;
    use mdkg_run::env::Mode;

    #[test]
    fn an_interactive_receiver_reports_its_session_whatever_the_policy() {
        assert_eq!(EnvMode::of(Mode::Allowlist, false), EnvMode::Allowlist);
        assert_eq!(EnvMode::of(Mode::Inherit, false), EnvMode::Inherit);
        assert_eq!(EnvMode::of(Mode::Allowlist, true), EnvMode::Session);
        assert_eq!(EnvMode::of(Mode::Inherit, true), EnvMode::Session);
    }
}
