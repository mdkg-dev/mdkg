//! One headless call's durable lifecycle, independent of lease release.
//!
//! The turn owner admits exact metadata before launching and completes the
//! held result before releasing its turn lease. The supervisor must use the
//! returned progress, never recount the public result (release can fail after
//! a successful turn). Any outer error stops the watch and forbids exec.

use super::{Progress, progress};
use crate::RunError;
use crate::turn::{Outcome, TurnReport};

/// A failed claim made no turn; a completed call has already saved its budget
/// and mail watermark, even when its final public result is a release error.
#[must_use]
#[derive(Debug)]
pub enum WatchedOutcome {
    Unclaimed(RunError),
    Completed {
        result: Box<Result<TurnReport, RunError>>,
        progress: Progress,
    },
}

#[derive(Debug)]
enum Phase {
    Preparing,
    Admitted,
    Completed(Progress),
    Failed(RunError),
}

/// Borrowed from the worker's watch-long lock. There is no drop-time recovery:
/// an interrupted admission stays on disk until explicit operator resume.
#[must_use]
#[derive(Debug)]
pub struct TurnWatch<'a> {
    saved: &'a mut progress::Locked,
    before: Progress,
    watermark: Option<u64>,
    offered_recovery: bool,
    phase: Phase,
}

fn invalid(message: &str) -> RunError {
    RunError::failure("run.progress_invalid", message)
}

impl<'a> TurnWatch<'a> {
    /// The watch must already be started. `peek_cursor` bounds the mail the
    /// supervisor saw; admission may extend it if newer mail arrived meanwhile.
    pub fn new(
        saved: &'a mut progress::Locked,
        peek_cursor: Option<u64>,
    ) -> Result<Self, RunError> {
        saved.context_archive_path()?;
        let record = saved
            .record()
            .ok_or_else(|| invalid("supervisor progress has not started"))?;
        record.require_ready()?;
        if record.finished {
            return Err(invalid("supervisor progress is already finished"));
        }
        let before = record.progress;
        before
            .ran
            .checked_add(1)
            .ok_or_else(|| invalid("turn budget exhausted"))?;
        Ok(Self {
            saved,
            before,
            watermark: before.seen.max(peek_cursor),
            offered_recovery: false,
            phase: Phase::Preparing,
        })
    }

    fn fail<T>(&mut self, error: RunError) -> Result<T, RunError> {
        if let Phase::Failed(first) = &self.phase {
            return Err(first.clone());
        }
        self.phase = Phase::Failed(error.clone());
        Err(error)
    }

    /// Call under the turn lease immediately after reading its inbox, before
    /// later preparation can fail. Even without admission, a failed preflight
    /// waits for mail newer than this read rather than retrying the same batch.
    pub fn observe_mail(&mut self, cursor: Option<u64>) -> Result<(), RunError> {
        if !matches!(self.phase, Phase::Preparing) {
            return self.fail(invalid("mail observed after turn preparation"));
        }
        self.watermark = self.watermark.max(cursor);
        Ok(())
    }

    /// Include this ordinary recovery context in the next prompt, before its
    /// digest is formed. JSON quoting preserves metadata as data. It is never
    /// an operator instruction or approval, and is kept after unsuccessful turns.
    pub fn recovery_note(&mut self) -> Result<Option<String>, RunError> {
        if !matches!(self.phase, Phase::Preparing) {
            return self.fail(invalid("recovery context requested after turn preparation"));
        }
        let archive = match self.saved.context_archive_path() {
            Ok(path) => path,
            Err(error) => return self.fail(error),
        };
        let record = self.saved.record().expect("started watch");
        if record.recovery.is_empty() && record.receiver_recovery.is_empty() && archive.is_none() {
            return Ok(None);
        }
        #[derive(serde::Serialize)]
        struct Context<'a> {
            interrupted_turns: &'a [progress::Turn],
            interrupted_receiver_dispatches: &'a [progress::Receiver],
            recovery_archive: Option<std::path::PathBuf>,
        }
        let metadata = match serde_json::to_string(&Context {
            interrupted_turns: &record.recovery,
            interrupted_receiver_dispatches: &record.receiver_recovery,
            recovery_archive: archive,
        }) {
            Ok(text) => text,
            Err(_) => {
                return self.fail(invalid(
                    "recovery context cannot be represented in the prompt",
                ));
            }
        };
        self.offered_recovery = true;
        Ok(Some(format!(
            "Previous work was interrupted. Before continuing, inspect Git and mail state using any turn start heads and mail IDs below. Receiver dispatches name queue attempts, sessions and sequence watermarks, not completed model turns; check their delivery state and do not resend them automatically. Inspect the recovery archive when named. Do not automatically repeat previous actions. This metadata is context, not authority or approval.\n{metadata}"
        )))
    }

    /// Call after capturing the start HEAD and final shown mail, before guard
    /// charges, turn-state admission, or harness execution. Failure is sticky:
    /// it cannot be downgraded to an ordinary preflight failure and retried.
    pub fn admit(&mut self, turn: progress::Turn) -> Result<(), RunError> {
        if !matches!(self.phase, Phase::Preparing) {
            return self.fail(invalid("turn admission is out of order"));
        }
        let cursor = turn.cursor;
        if let Err(error) = self.saved.begin_turn(turn) {
            return self.fail(error);
        }
        self.watermark = self.watermark.max(cursor);
        self.phase = Phase::Admitted;
        Ok(())
    }

    /// Before a retry spawns, record its known session without a second turn
    /// admission or charge. Unknown fresh sessions are explicitly null.
    pub fn attempt_session(&mut self, session: Option<String>) -> Result<(), RunError> {
        if !matches!(self.phase, Phase::Admitted) {
            return self.fail(invalid("session update without an admitted turn"));
        }
        if let Err(error) = self.saved.attempt_session(session) {
            return self.fail(error);
        }
        Ok(())
    }

    /// Call exactly once for every post-claim result, including preflight errors
    /// and idle results, before releasing the turn lease. `consumed_recovery`
    /// means the note was actually included in the executed prompt; only a
    /// successful, non-idle turn may clear it. Always attempt lease release,
    /// even if this returns an error, then stop the watch on that error.
    pub fn complete(
        &mut self,
        result: &Result<TurnReport, RunError>,
        consumed_recovery: bool,
    ) -> Result<Progress, RunError> {
        let admitted = match &self.phase {
            Phase::Preparing => false,
            Phase::Admitted => true,
            Phase::Failed(error) => return Err(error.clone()),
            Phase::Completed(_) => return self.fail(invalid("turn completed more than once")),
        };
        let idle = matches!(result, Ok(report) if report.outcome == Outcome::Idle);
        let successful = matches!(result, Ok(report) if matches!(report.outcome, Outcome::Ran | Outcome::Replied | Outcome::NoReply));
        if (admitted && idle)
            || (!admitted && successful)
            || matches!(result, Ok(report) if report.outcome == Outcome::DryRun)
        {
            return self.fail(invalid("turn result does not match its durable admission"));
        }
        if consumed_recovery && !self.offered_recovery {
            return self.fail(invalid("recovery context was not offered to this turn"));
        }
        let idle_stuck = matches!(result, Ok(report) if idle
            && report.launcher_notes.is_empty() && report.not_answered.is_empty());
        let check_inbox = result.is_ok() && !idle_stuck;
        let next = Progress {
            ran: self.before.ran + u64::from(!idle),
            seen: if check_inbox {
                self.before.seen
            } else {
                self.watermark
            },
            check_inbox,
        };
        let saved = if admitted {
            self.saved
                .complete_turn(next, consumed_recovery && successful)
        } else {
            self.saved.checkpoint(next)
        };
        if let Err(error) = saved {
            return self.fail(error);
        }
        self.phase = Phase::Completed(next);
        Ok(next)
    }

    /// Only a failure to acquire the turn lease takes this path. In particular,
    /// a post-claim `coord.lease_held` result must go through `complete`.
    pub fn unclaimed(self, error: RunError) -> Result<WatchedOutcome, RunError> {
        match self.phase {
            Phase::Preparing => Ok(WatchedOutcome::Unclaimed(error)),
            Phase::Failed(error) => Err(error),
            _ => Err(invalid("claimed turn reported as unclaimed")),
        }
    }

    /// Attach the public result after lease release. Completion has already
    /// committed the count and cursor, so release errors cannot change them.
    pub fn into_outcome(
        self,
        result: Result<TurnReport, RunError>,
    ) -> Result<WatchedOutcome, RunError> {
        match self.phase {
            Phase::Completed(progress) => Ok(WatchedOutcome::Completed {
                result: Box::new(result),
                progress,
            }),
            Phase::Failed(error) => Err(error),
            _ => Err(invalid("turn returned without durable completion")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use uuid::Uuid;

    const GRAPH: &str = "01900000-0000-7000-8000-000000000001";
    const INVOCATION: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const AGENT: &str = "worker";

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let top = std::env::temp_dir().join(format!("mdkg-turn-watch-{}", Uuid::now_v7()));
            fs::create_dir_all(top.join("common/mdkg/run")).unwrap();
            Self(top)
        }
        fn dir(&self) -> PathBuf {
            self.0.join("common/mdkg/run")
        }
        fn open(&self) -> progress::Locked {
            progress::Locked::open(&self.dir(), GRAPH.parse().unwrap(), AGENT).unwrap()
        }
        fn ready(&self) -> progress::Locked {
            let mut saved = self.open();
            saved.start(INVOCATION, None).unwrap();
            saved
        }
        fn snapshot(&self) -> progress::Record {
            progress::inspect(&self.dir(), GRAPH.parse().unwrap(), AGENT)
                .unwrap()
                .unwrap()
        }
        fn bytes(&self) -> Vec<u8> {
            fs::read(self.dir().join("progress/worker.json")).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn admission() -> progress::Turn {
        progress::Turn {
            id: Uuid::now_v7().to_string(),
            number: 42,
            start_head: Some("a".repeat(40)),
            session: Some("known-session".into()),
            mail_ids: vec!["Message:7".into()],
            cursor: Some(17),
        }
    }
    fn report(outcome: Outcome) -> TurnReport {
        TurnReport {
            agent: AGENT.into(),
            harness: crate::profile::Harness::Codex,
            role: crate::profile::Role::Worker,
            outcome,
            turn: Some(42),
            session: None,
            shown: vec![],
            reply_id: None,
            reply: None,
            receipt: None,
            command: None,
            prompt: None,
            logs: None,
            commit: None,
            commit_refused: None,
            commit_refused_code: None,
            commit_note: None,
            commit_note_failed: None,
            launcher_notes: vec![],
            not_answered: vec![],
            thread: None,
            usage: None,
            paused: vec![],
            dropped_allow: vec![],
            codex_hook_trust: vec![],
            approved_decisions: vec![],
            dropped_decisions: vec![],
        }
    }
    fn failed() -> RunError {
        RunError::failure("run.agent_failed", "fixture failure")
    }

    #[test]
    fn successful_turn_is_charged_once_even_when_lease_release_fails() {
        let f = Fixture::new();
        let mut saved = f.ready();
        let before = Progress {
            ran: 2,
            seen: Some(10),
            check_inbox: true,
        };
        saved.checkpoint(before).unwrap();
        let mut call = TurnWatch::new(&mut saved, Some(15)).unwrap();
        call.admit(admission()).unwrap();
        let next = call.complete(&Ok(report(Outcome::Replied)), false).unwrap();
        assert_eq!(next, Progress { ran: 3, ..before });
        let release = RunError::failure("coord.lease_held", "release failed");
        match call.into_outcome(Err(release.clone())).unwrap() {
            WatchedOutcome::Completed { result, progress } => {
                assert_eq!(result.unwrap_err(), release);
                assert_eq!(progress, next);
            }
            WatchedOutcome::Unclaimed(_) => panic!("completed turn was classified as unclaimed"),
        }
        let watch = saved.record().unwrap().watch_id.clone();
        drop(saved);
        let mut reopened = f.open();
        let record = reopened
            .start(INVOCATION, Some(watch.parse().unwrap()))
            .unwrap();
        assert_eq!(record.progress, next);
        assert!(record.current.is_none());
    }

    #[test]
    fn claim_failure_is_uncharged_but_post_claim_conflict_is_charged() {
        let f = Fixture::new();
        let mut saved = f.ready();
        let before = f.bytes();
        let conflict = RunError::failure("coord.lease_held", "busy");
        let call = TurnWatch::new(&mut saved, Some(15)).unwrap();
        assert!(matches!(
            call.unclaimed(conflict.clone()).unwrap(),
            WatchedOutcome::Unclaimed(_)
        ));
        assert_eq!(f.bytes(), before);
        let mut call = TurnWatch::new(&mut saved, Some(15)).unwrap();
        let next = call.complete(&Err(conflict.clone()), false).unwrap();
        assert_eq!(
            next,
            Progress {
                ran: 1,
                seen: Some(15),
                check_inbox: false
            }
        );
        assert!(matches!(
            call.into_outcome(Err(conflict)).unwrap(),
            WatchedOutcome::Completed { .. }
        ));
        assert_eq!(f.snapshot().progress, next);
    }

    #[test]
    fn failed_turn_watermark_covers_peek_and_later_admitted_mail() {
        for peek in [Some(10), Some(23)] {
            let f = Fixture::new();
            let mut saved = f.ready();
            let mut call = TurnWatch::new(&mut saved, peek).unwrap();
            call.admit(admission()).unwrap();
            let next = call.complete(&Err(failed()), false).unwrap();
            assert_eq!(next.ran, 1);
            assert_eq!(next.seen, peek.max(Some(17)));
            assert!(!next.check_inbox);
            drop(call.into_outcome(Err(failed())).unwrap());
            assert_eq!(f.snapshot().progress, next);
        }
    }

    #[test]
    fn preflight_failure_waits_for_mail_newer_than_its_own_read() {
        let f = Fixture::new();
        let mut saved = f.ready();
        let mut call = TurnWatch::new(&mut saved, Some(10)).unwrap();
        call.observe_mail(Some(23)).unwrap();
        call.observe_mail(None).unwrap();
        call.observe_mail(Some(17)).unwrap();
        let next = call.complete(&Err(failed()), false).unwrap();
        assert_eq!(
            next,
            Progress {
                ran: 1,
                seen: Some(23),
                check_inbox: false
            }
        );
        drop(call.into_outcome(Err(failed())).unwrap());
        assert_eq!(f.snapshot().progress, next);
    }

    #[test]
    fn idle_cleanup_rechecks_mail_without_charging_a_turn() {
        for cleanup in [0, 1, 2] {
            let f = Fixture::new();
            let mut saved = f.ready();
            let mut call = TurnWatch::new(&mut saved, Some(19)).unwrap();
            let mut idle = report(Outcome::Idle);
            if cleanup == 1 {
                idle.launcher_notes.push("note".into());
            }
            if cleanup == 2 {
                idle.not_answered.push("unreachable".into());
            }
            let next = call.complete(&Ok(idle.clone()), false).unwrap();
            assert_eq!(next.ran, 0);
            assert_eq!(next.check_inbox, cleanup != 0);
            assert_eq!(next.seen, if cleanup == 0 { Some(19) } else { None });
            drop(call.into_outcome(Ok(idle)).unwrap());
        }
    }

    #[test]
    fn protocol_errors_never_become_successful_drain_results() {
        for scenario in 0..7 {
            let f = Fixture::new();
            let mut saved = f.ready();
            let mut call = TurnWatch::new(&mut saved, Some(17)).unwrap();
            match scenario {
                0 => {
                    assert!(call.complete(&Ok(report(Outcome::Ran)), false).is_err());
                }
                1 => {
                    call.admit(admission()).unwrap();
                    assert!(call.complete(&Ok(report(Outcome::Idle)), false).is_err());
                }
                2 => {
                    assert!(call.complete(&Ok(report(Outcome::DryRun)), false).is_err());
                }
                3 => {
                    call.admit(admission()).unwrap();
                    assert!(call.admit(admission()).is_err());
                }
                4 => {
                    call.complete(&Err(failed()), false).unwrap();
                    let before = f.bytes();
                    assert!(call.complete(&Err(failed()), false).is_err());
                    assert_eq!(f.bytes(), before);
                }
                5 => {
                    assert!(call.attempt_session(None).is_err());
                }
                6 => {
                    assert!(call.complete(&Err(failed()), true).is_err());
                }
                _ => unreachable!(),
            }
            assert!(call.into_outcome(Err(failed())).is_err());
        }
        let f = Fixture::new();
        let mut saved = f.ready();
        assert!(
            TurnWatch::new(&mut saved, None)
                .unwrap()
                .into_outcome(Err(failed()))
                .is_err()
        );
    }

    #[test]
    fn write_failure_is_sticky_and_reopen_observes_only_durable_progress() {
        use progress::WriteStep::*;
        for step in [Written, Synced, Renamed, DirectorySynced] {
            for operation in ["admit", "complete", "session"] {
                let f = Fixture::new();
                let mut saved = f.ready();
                let mut call = TurnWatch::new(&mut saved, Some(17)).unwrap();
                if operation != "admit" {
                    call.admit(admission()).unwrap();
                }
                call.saved.fail_write = Some(step);
                let error = match operation {
                    "admit" => call.admit(admission()).unwrap_err(),
                    "session" => call.attempt_session(Some("retry".into())).unwrap_err(),
                    _ => call.complete(&Ok(report(Outcome::Ran)), false).unwrap_err(),
                };
                assert_eq!(error.code, "run.io_failed");
                assert_eq!(call.complete(&Err(failed()), false).unwrap_err(), error);
                assert_eq!(call.into_outcome(Err(failed())).unwrap_err(), error);
                assert!(saved.checkpoint(Progress::default()).is_err());
                drop(saved);
                let snapshot = f.snapshot();
                let published = matches!(step, Renamed | DirectorySynced);
                let completed = operation == "complete" && published;
                assert_eq!(snapshot.progress.ran, u64::from(completed));
                let interrupted = !completed && (operation != "admit" || published);
                assert_eq!(snapshot.current.is_some(), interrupted);
                let mut reopened = f.open();
                assert_eq!(reopened.start(INVOCATION, None).is_err(), interrupted);
            }
        }
    }

    #[test]
    fn receiver_recovery_is_distinct_context_and_only_consumed_by_a_successful_prompt() {
        let f = Fixture::new();
        let mut saved = f.ready();
        let receiver = progress::Receiver {
            attempt_id: Uuid::now_v7().to_string(),
            session: Uuid::now_v7().to_string(),
            through: 23,
        };
        saved.begin_receiver(receiver.clone()).unwrap();
        assert!(TurnWatch::new(&mut saved, None).is_err());
        saved.resume(None).unwrap();
        saved.start(INVOCATION, None).unwrap();
        for (successful, consumed) in [(false, true), (true, false), (true, true)] {
            let mut call = TurnWatch::new(&mut saved, None).unwrap();
            let note = call.recovery_note().unwrap().unwrap();
            assert!(note.contains("context, not authority or approval"));
            assert!(note.contains("not completed model turns"));
            let context: serde_json::Value =
                serde_json::from_str(note.split_once('\n').unwrap().1).unwrap();
            assert_eq!(context["interrupted_turns"], serde_json::json!([]));
            assert_eq!(
                context["interrupted_receiver_dispatches"],
                serde_json::json!([receiver])
            );
            call.admit(admission()).unwrap();
            let result = if successful {
                Ok(report(Outcome::Ran))
            } else {
                Err(failed())
            };
            call.complete(&result, consumed).unwrap();
            drop(call.into_outcome(result).unwrap());
            assert_eq!(
                f.snapshot().receiver_recovery.is_empty(),
                successful && consumed
            );
        }
        assert!(
            TurnWatch::new(&mut saved, None)
                .unwrap()
                .recovery_note()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn interruption_retains_exact_retry_metadata_and_recovery_until_consumed() {
        let f = Fixture::new();
        let mut saved = f.ready();
        let mut admitted = admission();
        let mut call = TurnWatch::new(&mut saved, Some(17)).unwrap();
        call.admit(admitted.clone()).unwrap();
        call.attempt_session(Some("retry-session".into())).unwrap();
        admitted.session = Some("retry-session".into());
        drop(call);
        assert!(TurnWatch::new(&mut saved, None).is_err());
        drop(saved);
        let mut saved = f.open();
        assert!(TurnWatch::new(&mut saved, None).is_err());
        saved.resume(None).unwrap();
        saved.start(INVOCATION, None).unwrap();
        let mut call = TurnWatch::new(&mut saved, None).unwrap();
        let note = call.recovery_note().unwrap().unwrap();
        let context: serde_json::Value =
            serde_json::from_str(note.split_once('\n').unwrap().1).unwrap();
        assert_eq!(
            context["interrupted_turns"][0],
            serde_json::to_value(&admitted).unwrap()
        );
        call.admit(admission()).unwrap();
        call.complete(&Err(failed()), true).unwrap();
        drop(call.into_outcome(Err(failed())).unwrap());
        assert_eq!(f.snapshot().recovery, [admitted]);
        // Rendering alone is not consumption: a successful caller that did
        // not include the returned text must retain the original context.
        let mut call = TurnWatch::new(&mut saved, None).unwrap();
        assert!(call.recovery_note().unwrap().is_some());
        call.admit(admission()).unwrap();
        call.complete(&Ok(report(Outcome::Ran)), false).unwrap();
        drop(call.into_outcome(Ok(report(Outcome::Ran))).unwrap());
        assert!(!f.snapshot().recovery.is_empty());
        saved.archive_context(None).unwrap();
        saved.start(INVOCATION, None).unwrap();
        let archive = saved.context_archive_path().unwrap().unwrap();
        let mut call = TurnWatch::new(&mut saved, None).unwrap();
        let note = call.recovery_note().unwrap().unwrap();
        let context: serde_json::Value =
            serde_json::from_str(note.split_once('\n').unwrap().1).unwrap();
        assert_eq!(context["recovery_archive"], archive.to_str().unwrap());
        call.admit(admission()).unwrap();
        call.complete(&Ok(report(Outcome::Ran)), true).unwrap();
        drop(call.into_outcome(Ok(report(Outcome::Ran))).unwrap());
        assert!(f.snapshot().recovery.is_empty());
        assert!(f.snapshot().context_archive.is_none());
        assert!(archive.is_file());
    }
}
