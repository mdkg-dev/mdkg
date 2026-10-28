//! Interactive receiver admission, durable routing and bounded queue dispatch.
//! Agent specs select headless mode; absence selects interactive intent, never
//! a fallback headless launch. Only proven failures before queue starts can
//! release the shared reminder reservation.

use std::fs;
use std::path::Path;

use mdkg_coord_sqlite::git;
use serde_json::Value;
use uuid::Uuid;

use crate::hooks::{self, Mail as Header};
use crate::{Mail, RunError, state, turn};

mod dispatch;
mod evidence;
#[doc(hidden)]
pub mod guard;
pub mod route;
pub use dispatch::{Report, WakeStatus, supervise};

/// Proposed mode seam: a saved launcher spec or a named configuration entry
/// selects headless. With neither, the actor waits for interactive registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Headless,
    Interactive,
}

fn invalid(message: &str) -> RunError {
    RunError::failure(crate::error::CODE_UNKNOWN_AGENT, message)
}

/// Missing is distinct from unreadable. Do not follow links or block on
/// special files, and do not read an unbounded state/configuration file.
fn read_json(path: &Path) -> Result<Option<Value>, RunError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RunError::io("can't inspect receiver mode evidence", e)),
    };
    if !meta.is_file() {
        return Err(invalid("receiver mode evidence isn't a regular file"));
    }
    const CAP: u64 = 1 << 20;
    let bytes = git::read_capped(path, CAP + 1)
        .map_err(|e| RunError::io("can't read receiver mode evidence", e))?;
    if bytes.len() as u64 > CAP {
        return Err(invalid("receiver mode evidence is too large"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| invalid("receiver mode evidence isn't valid JSON"))
}

pub fn mode(root: &Path, dir: &Path, actor: &str) -> Result<Mode, RunError> {
    state::check_name(actor)?;
    let saved = match read_json(&state::state_path(dir, actor))? {
        None => false,
        Some(value) => {
            let agent: state::AgentState = serde_json::from_value(value)
                .map_err(|_| invalid("receiver mode evidence isn't valid launcher state"))?;
            if agent.spec.name != actor {
                return Err(invalid("receiver mode evidence names another actor"));
            }
            true
        }
    };
    let cfg = read_json(&root.join(".mdkg/config.json"))?;
    if cfg.as_ref().is_some_and(|c| !c.is_object()) {
        return Err(invalid("receiver configuration isn't an object"));
    }
    let mut cfg = cfg.unwrap_or(Value::Null);
    crate::orchestration::agent::merge(root, &mut cfg)?;
    let configured = match cfg.get("agents") {
        None | Some(Value::Null) => false,
        Some(Value::Object(agents)) => match agents.get(actor).filter(|_| actor != "defaults") {
            None => false,
            Some(Value::Object(_)) => true,
            Some(_) => return Err(invalid("the actor's configuration isn't an object")),
        },
        Some(_) => return Err(invalid("agents configuration isn't an object")),
    };
    Ok(if saved || configured {
        Mode::Headless
    } else {
        Mode::Interactive
    })
}

/// Re-read mode only after acquiring the actor's existing turn lease. The
/// callback must finish its bounded admission/reservation/dispatch work before
/// returning; a decision returned from it is not an authorization to send
/// after this function releases the lease. Never call `turn::turn` inside it
/// (that function acquires the same non-reentrant lease).
///
/// This reuses the launcher's dead-holder reclaim, install exclusion and
/// holder record. Errors and unwinding release only this holder's record/lease.
/// Callers renew before admission and while a dispatch is running; a wait that
/// outlasts ownership aborts instead of reacquiring and using stale decisions.
pub fn with_mode<T>(
    mail: &mut dyn Mail,
    root: &Path,
    dir: &Path,
    actor: &str,
    cwd: &Path,
    f: impl FnOnce(Mode, &mut dyn Mail) -> Result<T, RunError>,
) -> Result<T, RunError> {
    with_mode_on_renewal_failure(mail, root, dir, actor, cwd, Err, f)
}

// A dispatcher can report proven non-delivery when initial renewal fails,
// before its callback is entered. Other admission/cleanup failures keep their
// ordinary error path, as do all callers of the public with_mode helper.
fn with_mode_on_renewal_failure<T>(
    mail: &mut dyn Mail,
    root: &Path,
    dir: &Path,
    actor: &str,
    cwd: &Path,
    renewal_failure: impl FnOnce(RunError) -> Result<T, RunError>,
    f: impl FnOnce(Mode, &mut dyn Mail) -> Result<T, RunError>,
) -> Result<T, RunError> {
    state::check_name(actor)?;
    turn::claim_turn(mail, dir, actor, cwd)?;
    let mut held = Held {
        mail,
        actor,
        dir,
        released: false,
    };
    let result = match held.mail.renew_held(&turn::lease_scope(actor)) {
        Ok(_) => state::not_installing(dir, cwd)
            .and_then(|()| mode(root, dir, actor))
            .and_then(|mode| f(mode, held.mail)),
        Err(e) => renewal_failure(e),
    };
    let released = held.release();
    let value = result?;
    released?;
    Ok(value)
}

struct Held<'a> {
    mail: &'a mut dyn Mail,
    actor: &'a str,
    dir: &'a Path,
    released: bool,
}

impl Held<'_> {
    fn release(&mut self) -> Result<(), RunError> {
        self.released = true;
        turn::release_turn(self.mail, self.dir, self.actor)
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.release();
        }
    }
}

/// Evidence from a future authoritative registration reader, evaluated under
/// the run lease. `Ready` requires validated identity and a complete conflict
/// check, plus an idle observation. A missing/broken record is never `Ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    Missing,
    Unreadable,
    Busy,
    Suspended,
    Ready(Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Headless,
    WaitForRegistration,
    Busy,
    Suspended,
    Queue(Uuid),
}

/// A headless actor never takes the interactive route; an interactive actor
/// never becomes headless because registration is absent, unreadable or busy.
pub fn admission(mode: Mode, registration: Registration) -> Admission {
    match (mode, registration) {
        (Mode::Headless, _) => Admission::Headless,
        (_, Registration::Missing | Registration::Unreadable) => Admission::WaitForRegistration,
        (_, Registration::Busy) => Admission::Busy,
        (_, Registration::Suspended) => Admission::Suspended,
        (_, Registration::Ready(session)) => Admission::Queue(session),
    }
}

/// One reserved payload. A successful or ambiguous queue launch consumes it.
/// Only a proven failure before launch may undo it with `unclaim_reminder`,
/// using `through` and the atomic `previous` value. Never undo it on Drop.
#[derive(Debug, PartialEq, Eq)]
pub struct Reserved {
    pub previous: u64,
    pub through: u64,
    pub line: String,
}

// Payload identity, not transport idempotency. Codex queue chooses its own
// clientUserMessageId. Keep each mail/thread pair recognizable even when a
// stale holder submits it again in a different batch or attempt.
fn reminder_id(session: &Uuid, seq: u64) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"mdkg-reminder-v1\0");
    hash.update(session.as_bytes());
    hash.update(seq.to_be_bytes());
    format!("mdkg-{:x}", hash.finalize())
}

/// After admission and the conversation loop-guard check, reserve the newest
/// wake-worthy header, then filter against the displaced watermark returned
/// *under the same lock*. A separate earlier read is not sufficient: a hook
/// may have claimed more mail in between. Inputs come from a complete bounded
/// no-mark inbox page (oldest first, or sorted here); no bodies are accepted.
///
/// `None` means no payload may be dispatched, including reservation failure.
/// Call only inside the run lease; do not claim while the session is busy.
pub fn reserve_payload(
    common: &Path,
    actor: &str,
    session: &Uuid,
    unread: &[Header],
) -> Option<Reserved> {
    state::check_name(actor).ok()?;
    let mut eligible: Vec<Header> = unread.iter().filter(|m| hooks::wakes(m)).cloned().collect();
    eligible.sort_by_key(|m| m.seq);
    eligible.dedup_by_key(|m| m.seq);
    let through = eligible.last()?.seq;
    let previous = hooks::reserve_reminder(common, actor, &session.to_string(), through)?;
    eligible.retain(|m| m.seq > previous);
    let mut line = hooks::mail_line(&eligible)?;
    line.push_str(" Reminder IDs (mail number = thread-specific ID): ");
    line.push_str(
        &eligible
            .iter()
            .map(|m| format!("{}={}", m.seq, reminder_id(session, m.seq)))
            .collect::<Vec<_>>()
            .join("; "),
    );
    Some(Reserved {
        previous,
        through,
        line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::{Import, Inbox, Note};
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::rc::Rc;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mdkg-receiver-{}", Uuid::now_v7()));
            fs::create_dir_all(path.join(".mdkg")).unwrap();
            Self(path)
        }
        fn dir(&self) -> PathBuf {
            self.0.join("run")
        }
        fn config(&self, text: &str) {
            fs::write(self.0.join(".mdkg/config.json"), text).unwrap();
        }
        fn saved(&self, name: &str) {
            let agent = state::resolve(
                name,
                None,
                state::SpecFlags {
                    harness: Some(crate::Harness::Codex),
                    role: Some(crate::Role::Worker),
                    ..Default::default()
                },
                &self.0,
            )
            .unwrap();
            state::save(&self.dir(), &agent).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct LeaseMail {
        held: Rc<Cell<bool>>,
        after_claim: Option<Box<dyn FnOnce()>>,
        release_fails: bool,
        releases: usize,
    }
    impl Mail for LeaseMail {
        fn claim(&mut self, scope: &str) -> Result<(), RunError> {
            assert_eq!(scope, "run:alice");
            if self.held.replace(true) {
                return Err(RunError::failure("coord.lease_held", "held"));
            }
            if let Some(f) = self.after_claim.take() {
                f();
            }
            Ok(())
        }
        fn renew_held(&mut self, _: &str) -> Result<u64, RunError> {
            if self.held.get() {
                Ok(state::now_ms() + 30_000)
            } else {
                Err(invalid("lease lost"))
            }
        }
        fn release(&mut self, _: &str) -> Result<(), RunError> {
            self.releases += 1;
            if self.release_fails {
                return Err(RunError::failure("run.mail_failed", "release"));
            }
            assert!(self.held.replace(false));
            Ok(())
        }
        fn holder(&self) -> String {
            "test-receiver".into()
        }
        fn inbox(&mut self, _: &str, _: usize) -> Result<Inbox, RunError> {
            panic!("no body reads")
        }
        fn wait(&mut self, _: &str, _: Option<u64>, _: u64) -> Result<Option<Inbox>, RunError> {
            panic!("no waits")
        }
        fn import(&mut self, _: &Import<'_>) -> Result<String, RunError> {
            panic!("no imports")
        }
        fn ack(&mut self, _: &str, _: &[String], _: &str) -> Result<(), RunError> {
            panic!("no ack")
        }
        fn release_as(&mut self, _: &str, _: &str) -> Result<(), RunError> {
            panic!("live holder")
        }
        fn holds(&mut self, _: &str, _: &str, _: Option<&str>) -> Result<bool, RunError> {
            panic!("no file claims")
        }
        fn note(&mut self, _: &Note<'_>) -> Result<String, RunError> {
            panic!("no notes")
        }
    }

    #[test]
    fn mode_comes_from_specs_not_registration_or_defaults() {
        let f = Fixture::new();
        assert_eq!(mode(&f.0, &f.dir(), "alice").unwrap(), Mode::Interactive);
        f.config(r#"{"agents":{"defaults":{"harness":"codex"}}}"#);
        assert_eq!(mode(&f.0, &f.dir(), "alice").unwrap(), Mode::Interactive);
        assert_eq!(mode(&f.0, &f.dir(), "defaults").unwrap(), Mode::Interactive);
        f.config(r#"{"agents":{"alice":{}}}"#);
        assert_eq!(mode(&f.0, &f.dir(), "alice").unwrap(), Mode::Headless);
        f.config("{}");
        f.saved("codex:alice");
        assert_eq!(mode(&f.0, &f.dir(), "codex:alice").unwrap(), Mode::Headless);
        assert_eq!(mode(&f.0, &f.dir(), "alice").unwrap(), Mode::Interactive);
    }

    #[test]
    fn mode_refuses_broken_evidence_instead_of_guessing() {
        let f = Fixture::new();
        for cfg in [
            "{",
            "[]",
            "null",
            r#"{"agents":[]}"#,
            r#"{"agents":{"alice":null}}"#,
        ] {
            f.config(cfg);
            assert!(mode(&f.0, &f.dir(), "alice").is_err(), "{cfg}");
        }
        f.config("{}");
        f.saved("bob");
        fs::copy(
            state::state_path(&f.dir(), "bob"),
            state::state_path(&f.dir(), "alice"),
        )
        .unwrap();
        assert!(mode(&f.0, &f.dir(), "alice").is_err());
        fs::write(state::state_path(&f.dir(), "alice"), "{").unwrap();
        assert!(mode(&f.0, &f.dir(), "alice").is_err());
        fs::write(
            state::state_path(&f.dir(), "alice"),
            vec![b' '; (1 << 20) + 1],
        )
        .unwrap();
        assert!(mode(&f.0, &f.dir(), "alice").is_err());
        assert!(mode(&f.0, &f.dir(), "../alice").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn mode_refuses_a_link_even_if_its_target_is_absent() {
        let f = Fixture::new();
        fs::create_dir_all(f.dir()).unwrap();
        std::os::unix::fs::symlink("missing", state::state_path(&f.dir(), "alice")).unwrap();
        assert!(mode(&f.0, &f.dir(), "alice").is_err());
    }

    #[test]
    fn mode_is_rechecked_after_acquiring_the_run_lease() {
        let f = Fixture::new();
        assert_eq!(mode(&f.0, &f.dir(), "alice").unwrap(), Mode::Interactive);
        let config = f.0.join(".mdkg/config.json");
        let mut mail = LeaseMail {
            after_claim: Some(Box::new(move || {
                fs::write(config, r#"{"agents":{"alice":{}}}"#).unwrap();
            })),
            ..Default::default()
        };
        let held = mail.held.clone();
        let observed = with_mode(&mut mail, &f.0, &f.dir(), "alice", &f.0, |m, _| {
            assert!(held.get());
            assert!(state::lease_record_path(&f.dir(), "alice").exists());
            Ok(m)
        })
        .unwrap();
        assert_eq!(observed, Mode::Headless);
        assert_eq!(mail.releases, 1);
        assert!(!held.get());
        assert!(!state::lease_record_path(&f.dir(), "alice").exists());
    }

    #[test]
    fn cleanup_after_takeover_preserves_replacement_record() {
        let f = Fixture::new();
        let mut mail = LeaseMail::default();
        let replacement = turn::lease_record("replacement@run-test".into());
        let path = state::lease_record_path(&f.dir(), "alice");
        with_mode(&mut mail, &f.0, &f.dir(), "alice", &f.0, |_, _| {
            fs::write(&path, serde_json::to_vec(&replacement).unwrap()).unwrap();
            Ok(())
        })
        .unwrap();
        let after: state::LeaseRecord = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(after, replacement);
    }

    #[test]
    fn admission_errors_and_unwinding_release_the_lease() {
        let f = Fixture::new();
        let mut mail = LeaseMail::default();
        f.config("{");
        assert!(
            with_mode(
                &mut mail,
                &f.0,
                &f.dir(),
                "alice",
                &f.0,
                |_, _| -> Result<(), RunError> { panic!("must not enter on unreadable evidence") }
            )
            .is_err()
        );
        assert_eq!(mail.releases, 1);
        f.config("{}");
        assert!(
            with_mode(
                &mut mail,
                &f.0,
                &f.dir(),
                "alice",
                &f.0,
                |_, _| -> Result<(), RunError> { Err(invalid("registration unavailable")) }
            )
            .is_err()
        );
        assert_eq!(mail.releases, 2);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<(), RunError> =
                with_mode(&mut mail, &f.0, &f.dir(), "alice", &f.0, |_, _| {
                    panic!("dispatch failure")
                });
        }));
        assert!(panic.is_err());
        assert_eq!(mail.releases, 3);
        assert!(!mail.held.get());
    }

    #[test]
    fn a_conflict_never_enters_admission_and_release_failures_are_reported() {
        let f = Fixture::new();
        let mut mail = LeaseMail::default();
        mail.held.set(true);
        assert!(
            with_mode(
                &mut mail,
                &f.0,
                &f.dir(),
                "alice",
                &f.0,
                |_, _| -> Result<(), RunError> { panic!("lease not held") }
            )
            .is_err()
        );
        assert_eq!(mail.releases, 0);
        mail.held.set(false);
        mail.release_fails = true;
        assert_eq!(
            with_mode(&mut mail, &f.0, &f.dir(), "alice", &f.0, |_, _| Ok(()))
                .unwrap_err()
                .message,
            "release"
        );
    }

    #[test]
    fn interactive_evidence_never_falls_back_to_headless() {
        let session = Uuid::now_v7();
        for (registration, expected) in [
            (Registration::Missing, Admission::WaitForRegistration),
            (Registration::Unreadable, Admission::WaitForRegistration),
            (Registration::Busy, Admission::Busy),
            (Registration::Suspended, Admission::Suspended),
            (Registration::Ready(session), Admission::Queue(session)),
        ] {
            assert_eq!(admission(Mode::Interactive, registration), expected);
            assert_eq!(admission(Mode::Headless, registration), Admission::Headless);
        }
    }

    fn header(seq: u64) -> Header {
        Header {
            seq,
            kind: "note".into(),
            sender: "alice".into(),
            room: None,
        }
    }

    #[test]
    fn ambiguous_six_then_seven_never_repeats_six_in_the_payload() {
        let f = Fixture::new();
        let session = Uuid::now_v7();
        let first = reserve_payload(&f.0, "alice", &session, &[header(6)]).unwrap();
        assert_eq!((first.previous, first.through), (0, 6));
        assert!(first.line.contains("#6 note"));
        // Process launched but no definite delivery result: drop without undo.
        drop(first);
        assert!(reserve_payload(&f.0, "alice", &session, &[header(6)]).is_none());
        let second = reserve_payload(&f.0, "alice", &session, &[header(7), header(6)]).unwrap();
        assert_eq!((second.previous, second.through), (6, 7));
        assert!(second.line.contains("#7 note"));
        assert!(!second.line.contains("#6"));
        assert!(!second.line.contains(&reminder_id(&session, 6)));
        assert!(second.line.contains(&reminder_id(&session, 7)));
    }

    #[test]
    fn reminder_ids_are_per_mail_and_thread_not_attempt_or_batch() {
        let f = Fixture::new();
        let other = Fixture::new();
        let session = Uuid::now_v7();
        let one = reserve_payload(&f.0, "alice", &session, &[header(6)]).unwrap();
        // Independent stale-holder state, same mail and thread, larger batch.
        let two = reserve_payload(&other.0, "alice", &session, &[header(6), header(7)]).unwrap();
        let id = reminder_id(&session, 6);
        assert!(one.line.contains(&format!("6={id}")));
        assert!(two.line.contains(&format!("6={id}")));
        assert_ne!(id, reminder_id(&session, 7));
        assert_ne!(id, reminder_id(&Uuid::now_v7(), 6));
        assert_eq!(id.len(), 69);
    }

    #[test]
    fn a_hook_between_preview_and_reservation_changes_the_payload_floor() {
        let f = Fixture::new();
        let session = Uuid::now_v7();
        let preview = hooks::reminded_through(&f.0, "alice", &session.to_string());
        assert_eq!(preview, 0);
        assert!(hooks::claim_reminder(
            &f.0,
            "alice",
            &session.to_string(),
            6
        ));
        let batch = reserve_payload(&f.0, "alice", &session, &[header(6), header(7)]).unwrap();
        assert_eq!(batch.previous, 6);
        assert!(!batch.line.contains("#6"));
    }

    #[test]
    fn proven_no_launch_can_undo_but_cannot_erase_a_newer_hook_claim() {
        let f = Fixture::new();
        let session = Uuid::now_v7();
        let s = session.to_string();
        let batch = reserve_payload(&f.0, "alice", &session, &[header(6)]).unwrap();
        assert!(hooks::unclaim_reminder(
            &f.0,
            "alice",
            &s,
            batch.through,
            batch.previous
        ));
        let retry = reserve_payload(&f.0, "alice", &session, &[header(6)]).unwrap();
        assert!(hooks::claim_reminder(&f.0, "alice", &s, 7));
        assert!(!hooks::unclaim_reminder(
            &f.0,
            "alice",
            &s,
            retry.through,
            retry.previous
        ));
        assert!(reserve_payload(&f.0, "alice", &session, &[header(6), header(7)]).is_none());
    }

    #[test]
    fn wake_filter_and_formatter_apply_before_dispatch() {
        let f = Fixture::new();
        let session = Uuid::now_v7();
        let mut room_note = header(99);
        room_note.room = Some("release".into());
        assert!(reserve_payload(&f.0, "alice", &session, &[room_note.clone()]).is_none());
        assert_eq!(
            hooks::reminded_through(&f.0, "alice", &session.to_string()),
            0
        );
        let mut review = header(7);
        review.room = Some("release".into());
        review.kind = "review_request".into();
        review.sender = "INJECT\nsecret".into();
        let batch = reserve_payload(
            &f.0,
            "alice",
            &session,
            &[room_note, review, header(6), header(6)],
        )
        .unwrap();
        assert_eq!(batch.through, 7);
        assert_eq!(batch.line.matches("#6").count(), 1);
        assert!(
            batch
                .line
                .contains("#7 review_request from a sender in room release")
        );
        assert!(!batch.line.contains("INJECT"));
        assert!(!batch.line.contains("#99"));
        assert!(batch.line.contains("senders' data"));
    }
}
