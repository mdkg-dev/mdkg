//! `agents stop --drain` (API v1 A18): a graceful stop from outside the
//! supervisor. It writes a stop request for each agent's running
//! supervisor ([`supervise::stop`]); the supervisor lets a turn in flight
//! finish, finishes the agent's watch and stops admitting its turns.
//! Nothing is killed.

use std::path::Path;
use std::time::{Duration, Instant};

use clap::Args;
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::{ErrorKind, GraphError};
use mdkg_run::state;
use mdkg_run::supervise::{self, registry};
use serde::Serialize;

use super::{configured, graph_id, known};
use crate::coord::Repo;
use crate::output::Report;
use crate::run::graph_error;

#[derive(Debug, Args)]
pub struct StopArgs {
    /// The agents to stop.
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub agents: Vec<String>,
    /// Every agent a running supervisor runs.
    #[arg(long)]
    pub all: bool,
    /// Let a turn in flight finish, then admit no more (required: the only
    /// mode).
    #[arg(long)]
    pub drain: bool,
    /// Reserved for a forced stop; refused until it's specified.
    #[arg(long, hide = true)]
    pub now: bool,
    /// Wait up to this many seconds for each agent to stop; exits 6 if a
    /// turn is still running then.
    #[arg(long)]
    pub wait: Option<u64>,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StopData {
    pub agents: Vec<StoppedAgent>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StoppedAgent {
    pub name: String,
    /// The supervisor asked to stop it; null when none runs it.
    pub supervisor_id: Option<String>,
    /// `draining` (a turn is still finishing, or the supervisor hasn't
    /// reached its next check), `stopped`, or `not_running` (no supervisor
    /// runs it, or it had already stopped).
    pub state: String,
    /// The turn it's finishing, by turn id, while draining.
    pub in_flight_turn: Option<String>,
}

/// Inside an agent's turn, `agents stop` may name only that agent
/// (orchestrator's decision on review #11899, option b): a worker turn
/// can't drain the other lanes or the orchestrator. The turn is found as
/// `use-key` finds it, failing closed: by the turn's mark in the
/// environment, and by a live turn of this repository anywhere in the
/// process chain, which `env -i` can't hide. Every turn found must be the
/// one agent named, and `--wait` is refused there: it would wait for the
/// caller's own turn to end.
fn only_itself_in_a_turn(repo: &Repo, a: &StopArgs) -> Result<(), GraphError> {
    let refuse = |why: String| {
        Err(GraphError::usage(
            Code::RunUnsupported,
            format!(
                "agents stop is refused in an agent turn unless it stops only that agent: {why}"
            ),
        )
        .with_hint("stop other agents from your own terminal"))
    };
    let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
        .map_err(|e| graph_error(mdkg_run::RunError::io("can't find git common dir", e)))?;
    let mut turns = Vec::new();
    if let Some(mark) = std::env::var_os(mdkg_run::profile::TURN_MARK).filter(|m| !m.is_empty()) {
        match mark
            .to_str()
            .and_then(|m| mdkg_run::hooks::live_turn_context(&common, m))
        {
            Some(turn) => turns.push(turn.actor),
            None => {
                return refuse(
                    "the environment carries a turn's mark that names no live turn".into(),
                );
            }
        }
    }
    match crate::hook::git::ancestor_turn_checked(&common) {
        Ok(None) => {}
        Ok(Some(turn)) => turns.push(turn),
        Err(why) => {
            return refuse(format!(
                "it can't rule out running inside a turn ({why}); run it from your own terminal"
            ));
        }
    }
    let Some(turn) = turns.first() else {
        return Ok(());
    };
    if turns.iter().any(|t| t != turn) {
        return refuse(format!("this runs under {}'s turns", turns.join(" and ")));
    }
    if a.all || a.agents.iter().any(|n| n != turn) {
        return refuse(format!("this runs under {turn}'s turn"));
    }
    if a.wait.is_some() {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            format!("--wait inside {turn}'s own turn would wait for that turn to end"),
        )
        .with_hint("drop --wait: the drain takes effect when this turn ends"));
    }
    Ok(())
}

/// How often `--wait` looks: `$MDKG_WAIT_POLL_MS`, else once a second.
fn poll_interval() -> Duration {
    let ms = std::env::var("MDKG_WAIT_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000u64);
    Duration::from_millis(ms.max(10))
}

pub(super) fn stop(
    root: &Path,
    actor: Option<String>,
    a: &StopArgs,
) -> Result<Report<StopData>, GraphError> {
    if a.now {
        return Err(GraphError::usage(
            Code::ApiUnsupported,
            "agents stop --now is reserved; this build stops only by draining",
        )
        .with_hint("use --drain: a turn in flight finishes, and nothing is killed"));
    }
    if !a.drain {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "agents stop needs --drain, its only mode",
        )
        .with_hint("a turn in flight finishes, then the agent admits no more turns"));
    }
    let repo = Repo::open(root)?;
    only_itself_in_a_turn(&repo, a)?;
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let requested_by = repo.actor(actor);
    let (found, _) = registry::list(&dir).map_err(graph_error)?;
    let names: Vec<String> = if a.all {
        let mut names: Vec<String> = found
            .iter()
            .filter(|f| f.live)
            .flat_map(|f| f.record.agents.keys().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
    } else {
        let snapshot = super::config(&repo)?;
        let mut defined = configured(&snapshot.effective.values);
        defined.extend(known(&dir));
        let mut names = Vec::new();
        for name in &a.agents {
            state::check_name(name).map_err(graph_error)?;
            let in_record = found.iter().any(|f| f.record.agents.contains_key(name));
            if !in_record && !defined.contains(name) {
                return Err(graph_error(mdkg_run::RunError::usage(
                    "run.unknown_agent",
                    format!("no agent named {name}"),
                )));
            }
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    };
    // Request first, for every agent, then watch them all.
    let mut agents = Vec::new();
    for name in names {
        let entry = match registry::running(&found, &name) {
            Some((record, state)) if state != registry::AgentState::Stopped => {
                let request = supervise::stop::Request::drain(
                    &requested_by,
                    supervise::timestamp(state::now_ms()),
                    &record.supervisor_id,
                );
                supervise::stop::write(&dir, &name, &request).map_err(graph_error)?;
                StoppedAgent {
                    name,
                    supervisor_id: Some(record.supervisor_id),
                    state: "draining".into(),
                    in_flight_turn: None,
                }
            }
            _ => StoppedAgent {
                name,
                supervisor_id: None,
                state: "not_running".into(),
                in_flight_turn: None,
            },
        };
        agents.push(entry);
    }
    let graph = graph_id(&repo).ok();
    let started = Instant::now();
    let limit = a.wait.map(Duration::from_secs);
    loop {
        settle(&dir, &mut agents)?;
        let draining = agents.iter().any(|a| a.state == "draining");
        let Some(limit) = limit.filter(|_| draining) else {
            break;
        };
        if started.elapsed() >= limit {
            break;
        }
        std::thread::sleep(poll_interval().min(limit.saturating_sub(started.elapsed())));
    }
    for agent in agents.iter_mut().filter(|a| a.state == "draining") {
        agent.in_flight_turn = graph.and_then(|graph| {
            supervise::progress::inspect_recovery(&dir, graph, &agent.name)
                .ok()
                .flatten()
                .and_then(|view| view.record.current.map(|turn| turn.id))
        });
    }
    let still: Vec<&str> = agents
        .iter()
        .filter(|a| a.state == "draining")
        .map(|a| a.name.as_str())
        .collect();
    if let Some(wait) = a.wait
        && !still.is_empty()
    {
        let message = format!(
            "still draining after {wait}s: {}; their turns finish on their own",
            still.join(", ")
        );
        return Ok(Report {
            data: Some(StopData { agents }),
            diagnostics: vec![
                Diagnostic::error(Code::MsgWaitTimeout, message)
                    .with_hint("run agents stop again with --wait, or read agents status"),
            ],
            failure: Some(ErrorKind::Timeout),
        });
    }
    Ok(Report::ok(StopData { agents }))
}

/// Moves each draining agent on to `stopped` once its supervisor marked it
/// so, or once the supervisor is gone having removed the request (it
/// stopped its last agent and exited). A supervisor gone with the request
/// still there died: the agent isn't running.
fn settle(dir: &Path, agents: &mut [StoppedAgent]) -> Result<(), GraphError> {
    let (found, _) = registry::list(dir).map_err(graph_error)?;
    for agent in agents.iter_mut().filter(|a| a.state == "draining") {
        let Some(id) = agent.supervisor_id.as_deref() else {
            continue;
        };
        let record = found
            .iter()
            .find(|f| f.live && f.record.supervisor_id == id);
        match record.map(|f| f.record.agents.get(&agent.name)) {
            Some(Some(registry::AgentState::Stopped)) => agent.state = "stopped".into(),
            Some(_) => (),
            None => {
                let pending = supervise::stop::read(dir, &agent.name)
                    .map_err(graph_error)?
                    .is_some_and(|r| r.supervisor_id == id);
                agent.state = if pending { "not_running" } else { "stopped" }.into();
            }
        }
    }
    Ok(())
}
