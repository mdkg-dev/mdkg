//! `agents turns <agent>`: an agent's turns from its turn index, newest
//! first, a page at a time (`--limit`, `--before <N>`), or one turn
//! (`--turn <N>`). Each turn's state and outcome are derived from its
//! evidence as `agents logs` derives them: the receipt, the index's end
//! line, the journal's `turn.ended`, then a live launcher.

use std::path::{Path, PathBuf};

use clap::Args;
use mdkg_format::Code;
use mdkg_graph::{ErrorKind, GraphError};
use mdkg_run::state;
use mdkg_run::supervise::registry;
use mdkg_run::turn::index::{self, Entry};
use mdkg_run::turn::journal;
use serde::Serialize;

use super::{configured, known};
use crate::coord::Repo;
use crate::output::Report;
use crate::run::graph_error;

/// The most turns one page holds.
pub const MAX_LIMIT: usize = 500;
const DEFAULT_LIMIT: usize = 50;

#[derive(Debug, Args)]
pub struct TurnsArgs {
    /// The agent's name.
    pub agent: String,
    /// At most this many turns, newest first (default 50, at most 500).
    #[arg(long)]
    pub limit: Option<usize>,
    /// Only turns numbered below N: pass the last page's `next_before`.
    #[arg(long, conflicts_with = "turn")]
    pub before: Option<u64>,
    /// Only turn number N.
    #[arg(long)]
    pub turn: Option<u64>,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// One turn.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TurnRow {
    /// The turn's id, a UUID: `agents logs <agent> --turn <turn_id>`.
    pub turn_id: String,
    /// Its number within the agent, from 1; never reused.
    pub number: u64,
    /// When its attempt was admitted (RFC 3339, UTC).
    pub started: String,
    /// When it ended, if its evidence records that.
    pub ended: Option<String>,
    /// `running`, `ended` or `interrupted`.
    pub state: String,
    /// Once it isn't running: `replied`, `no_reply`, `ran`, `idle`, `dry_run`,
    /// `failed`, `interrupted` or `not_started`; an open set. Null while
    /// running, or when no evidence names one.
    pub outcome: Option<String>,
    /// Its retained receipt, its lasting record; null when there's none
    /// (yet, or any more).
    pub receipt: Option<PathBuf>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TurnsData {
    pub agent: String,
    /// Newest first.
    pub turns: Vec<TurnRow>,
    /// Pass as `--before` for the next, older page; null on the last page
    /// and with `--turn`.
    pub next_before: Option<u64>,
}

fn row(dir: &Path, agent: &str, entry: &Entry) -> TurnRow {
    let id = &entry.start.turn_id;
    // The journal only when the index alone can't settle it.
    let events = entry
        .end
        .is_none()
        .then(|| journal::read(dir, agent, id).ok().flatten())
        .flatten();
    let derived = index::derive(dir, agent, entry, events.as_deref(), index::launcher_alive);
    let receipt = index::receipt_path(dir, agent, id);
    TurnRow {
        turn_id: id.clone(),
        number: entry.start.number,
        started: entry.start.started_at.clone(),
        ended: derived.ended_at,
        state: derived.state.into(),
        outcome: derived.outcome,
        receipt: receipt.is_file().then_some(receipt),
    }
}

pub(super) fn turns(root: &Path, a: &TurnsArgs) -> Result<TurnsData, GraphError> {
    state::check_name(&a.agent).map_err(graph_error)?;
    let limit = a.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(GraphError::usage(
            Code::UsageBadValue,
            format!("--limit takes 1 to {MAX_LIMIT}"),
        ));
    }
    let repo = Repo::open(root)?;
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let index = index::read(&dir, &a.agent).map_err(graph_error)?;
    if index.entries.is_empty() {
        let (found, _) = registry::list(&dir).unwrap_or_default();
        let defined = found.iter().any(|f| f.record.agents.contains_key(&a.agent))
            || known(&dir).contains(&a.agent)
            || configured(&super::config(&repo)?.effective.values).contains(&a.agent);
        if !defined {
            return Err(super::unknown_agent(&a.agent));
        }
    }
    if let Some(n) = a.turn {
        let entry = index.by_number(n).ok_or_else(|| {
            GraphError::new(
                ErrorKind::NotFound,
                mdkg_format::Diagnostic::error(
                    Code::ApiUnknownResource,
                    format!("{} has no turn {n}", a.agent),
                ),
            )
            .with_hint(format!("list them with `agents turns {}`", a.agent))
        })?;
        return Ok(TurnsData {
            agent: a.agent.clone(),
            turns: vec![row(&dir, &a.agent, entry)],
            next_before: None,
        });
    }
    let mut older = index
        .entries
        .iter()
        .rev()
        .filter(|e| a.before.is_none_or(|b| e.start.number < b));
    let page: Vec<&Entry> = older.by_ref().take(limit).collect();
    let next_before = match (older.next(), page.last()) {
        (Some(_), Some(last)) => Some(last.start.number),
        _ => None,
    };
    Ok(TurnsData {
        agent: a.agent.clone(),
        turns: page.into_iter().map(|e| row(&dir, &a.agent, e)).collect(),
        next_before,
    })
}

pub(super) fn report(root: &Path, a: &TurnsArgs) -> Report<TurnsData> {
    turns(root, a).map_or_else(Report::failed, Report::ok)
}
