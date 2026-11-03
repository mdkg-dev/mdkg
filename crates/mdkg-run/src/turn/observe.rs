//! What a client needs to re-attach to an agent (API v1 A18): its state,
//! its supervisor, its current and last turns, and `follow_from`, the
//! agent-follow position for this snapshot. Read-only: from the
//! supervisor records, the turn index and the journals, never the harness.

use std::path::Path;

use serde::Serialize;

use super::{index, journal};
use crate::builds::Build;
use crate::supervise::{registry, stop, timestamp};

/// The supervisor running the agent, or the last one whose record names
/// it when that one is gone (`stale`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct SupervisorView {
    pub supervisor_id: String,
    pub pid: u32,
    pub started_at: String,
    pub build: Option<Build>,
    /// `running` or `stale` (its record is there, its process is gone).
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct CurrentTurn {
    pub turn_id: String,
    pub number: u64,
    pub started_at: String,
    /// The latest `seq` in its output journal.
    pub output_after: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct LastTurn {
    pub turn_id: String,
    pub number: u64,
    /// `replied`, `ran`, `idle`, `dry_run`, `failed`, `interrupted` or
    /// `not_started`; an open set.
    pub outcome: Option<String>,
    pub ended_at: Option<String>,
}

/// One agent, for re-attaching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct AgentView {
    /// `stopped`, `idle`, `running`, `draining` or `paused`; an open set.
    pub state: String,
    pub supervisor: Option<SupervisorView>,
    pub current_turn: Option<CurrentTurn>,
    pub last_turn: Option<LastTurn>,
    /// Where `agents logs <agent> --follow --after` picks up: the running
    /// turn's `<turn_id>:<output_after>`, else the last turn's
    /// `<turn_id>:end`, else `turn:<N>` for its next turn.
    pub follow_from: String,
}

/// The latest `seq` in a turn's journal; 0 when there's none.
pub fn output_after(run_dir: &Path, agent: &str, turn_id: &str) -> u64 {
    journal::read(run_dir, agent, turn_id)
        .ok()
        .flatten()
        .and_then(|events| events.iter().filter_map(|e| e.seq).max())
        .unwrap_or(0)
}

/// The agent's state in its supervisor, from `found` (the supervisor
/// records, read once per snapshot). `paused` says the loop guard pauses
/// some of its threads.
pub fn agent_state(
    run_dir: &Path,
    agent: &str,
    found: &[registry::Found],
    running_turn: bool,
    paused: bool,
) -> String {
    match registry::running(found, agent) {
        None | Some((_, registry::AgentState::Stopped)) => "stopped",
        Some((record, _)) if stop::requested(run_dir, agent, &record.supervisor_id) => "draining",
        Some(_) if running_turn => "running",
        Some((_, registry::AgentState::Running)) => "running",
        Some(_) if paused => "paused",
        Some(_) => "idle",
    }
    .into()
}

/// The supervisor running `agent`, else the newest stale one naming it.
pub fn supervisor(found: &[registry::Found], agent: &str) -> Option<SupervisorView> {
    let view = |f: &registry::Found| SupervisorView {
        supervisor_id: f.record.supervisor_id.clone(),
        pid: f.record.pid,
        started_at: timestamp(f.record.started_ms),
        build: f.record.build.clone(),
        state: if f.live { "running" } else { "stale" }.into(),
    };
    found
        .iter()
        .rev()
        .find(|f| f.live && f.record.agents.contains_key(agent))
        .or_else(|| {
            found
                .iter()
                .rev()
                .find(|f| f.record.agents.contains_key(agent))
        })
        .map(view)
}

/// The agent's re-attach view.
pub fn agent(run_dir: &Path, agent: &str, found: &[registry::Found], paused: bool) -> AgentView {
    let turns = index::read(run_dir, agent).unwrap_or_default();
    let mut current = None;
    let mut last = None;
    for entry in turns.entries.iter().rev() {
        let events = journal::read(run_dir, agent, &entry.start.turn_id)
            .ok()
            .flatten();
        let derived = index::derive(run_dir, agent, entry, events.as_deref(), |s| {
            index::turn_running(run_dir, agent, s)
        });
        if derived.state == "running" {
            if current.is_none() && last.is_none() {
                current = Some(CurrentTurn {
                    turn_id: entry.start.turn_id.clone(),
                    number: entry.start.number,
                    started_at: entry.start.started_at.clone(),
                    output_after: events
                        .as_deref()
                        .and_then(|e| e.iter().filter_map(|e| e.seq).max())
                        .unwrap_or(0),
                });
            }
            continue;
        }
        last = Some(LastTurn {
            turn_id: entry.start.turn_id.clone(),
            number: entry.start.number,
            outcome: derived.outcome,
            ended_at: derived.ended_at,
        });
        break;
    }
    let follow_from = match (&current, &last) {
        (Some(c), _) => format!("{}:{}", c.turn_id, c.output_after),
        (None, Some(l)) => format!("{}:end", l.turn_id),
        (None, None) => format!("turn:{}", turns.next_number()),
    };
    AgentView {
        state: agent_state(run_dir, agent, found, current.is_some(), paused),
        supervisor: supervisor(found, agent),
        current_turn: current,
        last_turn: last,
        follow_from,
    }
}
