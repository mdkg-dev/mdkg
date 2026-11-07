//! `end` (the edd's turn.end): closes a turn in the kernel's order, A7's:
//! the shows a launcher-side failure takes back, `turn.ended` in the
//! journal, durable progress, the receipt, the index's end line, then the
//! run lease and the agent's clean path leases. A failed write never undoes
//! an earlier one, and never changes the turn's result.

use std::fs;
use std::path::Path;

use super::Turn;
use crate::error::RunError;
use crate::mail::Mail;
use crate::state::{self, AgentState};
use crate::turn::{TurnReport, index, journal, receipt, release_turn, remove_turn_files};

/// What `end` hands back: the turn's report with its receipt attached, and
/// the two later steps' results, each kept apart so the runner reports them
/// in today's order.
pub(crate) struct Closed {
    pub(crate) report: Result<TurnReport, RunError>,
    /// Durable progress's completion (D6).
    pub(crate) completed: Result<(), RunError>,
    /// The run lease's release.
    pub(crate) released: Result<(), RunError>,
}

/// The outcome an indexed attempt ends with: its report's, `failed` when
/// its harness run returned an error, `not_started` when nothing spawned.
fn indexed_outcome(
    result: &Result<TurnReport, RunError>,
    journal: Option<&journal::Writer>,
    spawned: bool,
) -> String {
    match result {
        Ok(report) => serde_json::to_value(report.outcome)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "ran".into()),
        Err(_) if spawned || journal.is_some_and(journal::Writer::harness_started) => {
            "failed".into()
        }
        Err(_) => "not_started".into(),
    }
}

/// Closes `lifecycle`'s turn with the runner's `result`.
pub(crate) fn end(
    mail: &mut dyn Mail,
    dir: &Path,
    agent: &mut AgentState,
    lifecycle: &mut Turn<'_, '_>,
    mut result: Result<TurnReport, RunError>,
) -> Closed {
    // A show stands only when the turn failed because of what it ran; a
    // launcher-side failure (the turn lease lost, run.mail_failed, journal
    // or receipt I/O, a timeout) takes it back, so load alone never sets
    // mail aside (dec-01a18ee0 item 8). Still under the turn lease. A
    // failed write of the take-back is accepted, like the window a
    // launcher crash leaves: the show stays counted, so it takes that
    // failure on top of two earlier counted shows to set the mail aside.
    // A set-aside acks only once its fault to `reports_to` is recorded:
    // sent, or kept as a local pending file with a warning when the send
    // fails (orchestrator #19789).
    if result.is_err() && !lifecycle.own_failure && !lifecycle.counted.is_empty() {
        let mut shows = state::Shows::load(dir, &agent.spec.name);
        shows.unshown(&lifecycle.counted);
        if let Err(e) = shows.save(dir, &agent.spec.name) {
            eprintln!(
                "warning: can't take back the mail's shows: {}",
                crate::digest::terminal_safe(&e.message)
            );
        }
    }
    // A7's order after the harness: `turn.ended` to the journal, the
    // receipt, D6 completion, then the index's end line. A failed write
    // never undoes an earlier one, and never changes the turn's result.
    let outcome = indexed_outcome(
        &result,
        lifecycle.journal.as_ref(),
        !lifecycle.process_exits.is_empty(),
    );
    if let Some(journal) = lifecycle.journal.as_mut()
        && let Err(e) = journal.ended(&outcome)
    {
        eprintln!("warning: {}", crate::digest::terminal_safe(&e.message));
    }
    // Save the held result before release. A release error cannot undo a
    // successful turn's count, and a failed save must still release the lease.
    let completed = if let Some(watch) = lifecycle.watch.as_mut() {
        watch
            .complete(
                &result,
                lifecycle.recovery_in_prompt && lifecycle.prompt_delivered,
            )
            .map(|_| ())
    } else {
        Ok(())
    };
    if let Some(admission) = lifecycle.receipt.take() {
        let status = admission.finish(mail, dir, agent, &result, &lifecycle.process_exits);
        // Only a durable full receipt may replace the fallback output. Keep
        // protection if retention or D6 completion failed. A failed harness
        // retains its logs until the next success, as before.
        if status.path.is_some()
            && completed.is_ok()
            && let Some((turns, keep)) = lifecycle.cleanup.take()
            && fs::remove_file(keep).is_ok()
            && let Ok(report) = &mut result
        {
            remove_turn_files(&turns);
            report.logs = None;
        }
        lifecycle.receipt_status = Some(status.clone());
        receipt::delivery::attach(&mut result, status);
    }
    if let Some(turn_id) = lifecycle.indexed.take()
        && let Err(e) = index::append(
            dir,
            &agent.spec.name,
            &index::Line::End(index::End {
                turn_id,
                ended_at: crate::supervise::timestamp(state::now_ms()),
                outcome,
            }),
        )
    {
        eprintln!("warning: {}", crate::digest::terminal_safe(&e.message));
    }
    let released = release_turn(mail, dir, &agent.spec.name);
    // Then the agent's clean path leases go, as the agent in its worktree
    // (step 5; orchestrator, #12639): a file lease protects uncommitted
    // edits only, so one on a file the turn committed doesn't wait out its
    // term. After the turn lease, whatever the turn's end; a failure only
    // warns.
    if lifecycle.claimed {
        let cleaned = mdkg_coord_sqlite::git::toplevel(&agent.spec.cwd)
            .map_err(|e| RunError::io("can't find the agent's worktree", e))
            .and_then(|top| mail.release_clean(&agent.spec.name, &top));
        if let Err(e) = cleaned {
            eprintln!(
                "warning: can't release {}'s clean leases: {}",
                crate::digest::clean(&agent.spec.name),
                crate::digest::terminal_safe(&e.message)
            );
        }
    }
    Closed {
        report: result,
        completed,
        released,
    }
}
