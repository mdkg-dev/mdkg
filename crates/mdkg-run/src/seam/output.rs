//! `output` (the edd's turn.output): the runner's normalized output events,
//! appended to the turn's journal in order. The kernel's own events
//! (`turn.started`, `harness.starting`, `turn.ended`) are written by its
//! other operations, never through here.
//!
//! In-process, the runner's live parser hands each line's events here as
//! it reads them; the first journal write that fails stops the batch and
//! is returned, and the runner stops journaling there.

use serde_json::Value;

use crate::error::RunError;
use crate::turn::journal;

/// Appends `events`, each a type and its data, to the turn's journal.
pub(crate) fn output(
    journal: &mut journal::Writer,
    events: impl IntoIterator<Item = (&'static str, Value)>,
) -> Result<(), RunError> {
    for (kind, data) in events {
        journal.write(kind, data, false)?;
    }
    Ok(())
}
