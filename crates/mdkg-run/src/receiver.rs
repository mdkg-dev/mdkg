//! Interactive receiver admission, durable routing and bounded queue dispatch.
//! Agent specs select headless mode; absence selects interactive intent, never
//! a fallback headless launch. Only proven failures before queue starts can
//! release the shared reminder reservation.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

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
mod wake;
pub use dispatch::{
    Report, WakeStatus, WatchedDispatch, dispatch_watched, supervise, supervise_durable,
};

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
    let cfg = crate::orchestration::load_agent_config(root)?;
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
    with_mode_inner(mail, (root, dir, actor, cwd), Some(START), Err, f)
}

/// How a start ([`with_mode`]) checks the lease it just claimed, and keeps
/// trying a check that timed out: each try's lease calls get `budget`
/// together, with a pause from `first`, doubling up to `most`, between
/// tries. No try starts after `total` from the first, the tries' own time
/// included, so the last can run up to its budget past it. Under load, a
/// supervisor's start failed its few-second check five times in a row and
/// exited (friction 87).
#[derive(Debug, Clone, Copy)]
struct Backoff {
    budget: Duration,
    first: Duration,
    most: Duration,
    total: Duration,
}

const START: Backoff = Backoff {
    budget: crate::mail::RECEIVER_START_BUDGET,
    first: Duration::from_secs(1),
    most: Duration::from_secs(15),
    total: Duration::from_secs(120),
};

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
    with_mode_inner(mail, (root, dir, actor, cwd), None, renewal_failure, f)
}

/// With `retry`, the first renewal gets its budget, and one that timed out
/// releases the lease and claims it again after a pause, until
/// `retry.total` is spent; each try is logged. Only before `f` runs:
/// renewals while it runs keep their budget.
fn with_mode_inner<T>(
    mail: &mut dyn Mail,
    (root, dir, actor, cwd): (&Path, &Path, &str, &Path),
    retry: Option<Backoff>,
    renewal_failure: impl FnOnce(RunError) -> Result<T, RunError>,
    f: impl FnOnce(Mode, &mut dyn Mail) -> Result<T, RunError>,
) -> Result<T, RunError> {
    state::check_name(actor)?;
    let begun = Instant::now();
    let budget = retry.map_or(crate::mail::RECEIVER_RENEWAL_BUDGET, |r| r.budget);
    let mut pause = retry.map_or(Duration::ZERO, |r| r.first);
    let mut tries = 1u32;
    loop {
        turn::claim_turn(mail, dir, actor, cwd)?;
        let mut held = Held {
            mail: &mut *mail,
            actor,
            dir,
            released: false,
        };
        let renewed = held
            .mail
            .renew_held_checked(&turn::lease_scope(actor), budget);
        if let (Err(e), Some(r)) = (&renewed, retry)
            && e.timed_out
            && begun.elapsed() + pause <= r.total
        {
            held.release()?;
            eprintln!(
                "warning: {actor}'s turn lease check timed out (try {tries}); trying again in {}s",
                pause.as_secs_f32()
            );
            std::thread::sleep(pause);
            pause = (pause * 2).min(r.most);
            tries += 1;
            continue;
        }
        let result = match renewed {
            Ok(_) => state::not_installing(dir, cwd)
                .and_then(|()| mode(root, dir, actor))
                .and_then(|mode| f(mode, held.mail)),
            Err(e) => renewal_failure(e.error),
        };
        let released = held.release();
        let value = result?;
        released?;
        return Ok(value);
    }
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

/// A header page as the wake filter reads it: a note replying to `actor`'s
/// own message is marked, so it never wakes the agent ([`hooks::wakes`]).
pub fn wake_headers(
    mail: &mut dyn Mail,
    actor: &str,
    page: Vec<crate::mail::Header>,
) -> Vec<Header> {
    page.into_iter()
        .map(|h| Header {
            own_reply: hooks::own_reply(&h.kind, &h.reference_ids, actor, |id| {
                sender_of(mail, actor, id)
            }),
            seq: h.seq,
            kind: h.kind,
            sender: h.sender,
            room: h.room,
        })
        .collect()
}

/// Senders found, by message id: a message's sender never changes, so a
/// note that doesn't wake isn't looked up again at every step. Bounded;
/// a lookup that fails isn't kept.
static SENDERS: std::sync::Mutex<Option<std::collections::HashMap<String, String>>> =
    std::sync::Mutex::new(None);

/// The most senders [`SENDERS`] keeps before it starts over.
const SENDERS_KEPT: usize = 4096;

fn sender_of(mail: &mut dyn Mail, actor: &str, id: &str) -> Option<String> {
    if let Ok(cache) = SENDERS.lock()
        && let Some(sender) = cache.as_ref().and_then(|c| c.get(id))
    {
        return Some(sender.clone());
    }
    let sender = mail.sender_of(actor, id).ok().flatten()?;
    if let Ok(mut cache) = SENDERS.lock() {
        let cache = cache.get_or_insert_with(Default::default);
        if cache.len() >= SENDERS_KEPT {
            cache.clear();
        }
        cache.insert(id.to_string(), sender.clone());
    }
    Some(sender)
}

/// The most header pages a wake reads past mail that doesn't wake.
const WAKE_PAGES: usize = 40;

/// The unread headers above `floor`, oldest first, as the wake filter reads
/// them: page after page (each at most `limit`) until one holds mail that
/// wakes, or none is left. Notes that never wake (replies to the agent's
/// own messages) stay unread and unreminded, so they'd otherwise fill
/// every page and hide what comes after them. Past [`WAKE_PAGES`] pages of
/// them with more left, it fails open: the newest note read wakes the
/// agent, whose reminder then moves the floor past them all.
pub fn wake_page(
    mail: &mut dyn Mail,
    actor: &str,
    floor: u64,
    limit: usize,
) -> Result<Vec<Header>, RunError> {
    wake_page_for(mail, actor, floor, limit, None)
}

pub(crate) fn wake_page_for(
    mail: &mut dyn Mail,
    actor: &str,
    floor: u64,
    limit: usize,
    kinds: Option<&[String]>,
) -> Result<Vec<Header>, RunError> {
    if kinds.is_some_and(<[String]>::is_empty) {
        return Ok(Vec::new());
    }
    let mut out: Vec<Header> = Vec::new();
    let mut after = floor;
    let mut pages = 0;
    loop {
        let page = mail.headers(actor, Some(after), limit)?;
        let more = page.more;
        let next = page.messages.iter().map(|h| h.seq).max().or(page.cursor);
        let headers = wake_headers(mail, actor, page.messages);
        let woke = headers.iter().any(|m| hooks::wakes_for(m, kinds));
        if kinds.is_some() {
            // Deferred headers remain unread. Do not retain every excluded
            // page or turn an excluded kind into a wake at the legacy cap.
            out.extend(headers.into_iter().filter(|m| hooks::wakes_for(m, kinds)));
        } else {
            // Notices remain readable in the mailbox, but cannot become
            // the legacy cap's fallback wake or hide a later actionable page.
            out.extend(
                headers
                    .into_iter()
                    .filter(|h| h.kind != mdkg_coord_sqlite::mail::NOTICE_KIND),
            );
        }
        match next {
            Some(n) if more && !woke && n > after => after = n,
            _ => return Ok(out),
        }
        pages += 1;
        if kinds.is_none() && pages >= WAKE_PAGES && !out.is_empty() {
            break;
        }
    }
    if let Some(last) = out.last_mut() {
        last.own_reply = false;
    }
    Ok(out)
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
    reserve_payload_for(common, actor, session, unread, None)
}

pub(crate) fn reserve_payload_for(
    common: &Path,
    actor: &str,
    session: &Uuid,
    unread: &[Header],
    kinds: Option<&[String]>,
) -> Option<Reserved> {
    state::check_name(actor).ok()?;
    let mut eligible: Vec<Header> = unread
        .iter()
        .filter(|m| hooks::wakes_for(m, kinds))
        .cloned()
        .collect();
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
            let path = fs::canonicalize(path).unwrap();
            Self::git(&path, &["init", "-q", "-b", "main"]);
            let fixture = Self(path);
            fixture.config("{}");
            fixture
        }
        fn dir(&self) -> PathBuf {
            self.0.join("run")
        }
        fn config(&self, text: &str) {
            Self::config_at(&self.0, text);
        }
        fn config_at(root: &Path, text: &str) {
            let mut value = serde_json::from_str::<Value>(text).ok();
            if let Some(Some(object)) = value.as_mut().map(Value::as_object_mut) {
                object.insert("schema_version".into(), 3.into());
                object.insert("format_version".into(), "0.7".into());
                object.insert(
                    "graph_id".into(),
                    "0192f3a0-0000-7000-8000-000000000000".into(),
                );
            }
            let bytes = value.map_or_else(|| text.to_owned(), |v| v.to_string());
            fs::write(root.join(".mdkg/config.json"), bytes).unwrap();
            Self::git(root, &["add", "--", ".mdkg/config.json"]);
            let changed = Self::git(root, &["diff", "--cached", "--quiet"]);
            match changed.status.code() {
                Some(0) => (),
                Some(1) => {
                    Self::git(root, &["commit", "-q", "-m", "graph"]);
                }
                _ => panic!("git diff failed"),
            }
        }
        fn git(root: &Path, args: &[&str]) -> std::process::Output {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_ATTR_NOSYSTEM", "1")
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "init.templateDir=",
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success() || args == ["diff", "--cached", "--quiet"],
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
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
        /// The next renewals that time out unanswered (friction 87).
        timeouts: u32,
        /// The next renewals that fail with an answer.
        refusals: u32,
        claims: usize,
        /// The budget each renewal was given.
        budgets: Vec<Duration>,
        /// How long each renewal takes.
        slow: Duration,
    }
    impl Mail for LeaseMail {
        fn claim(&mut self, scope: &str) -> Result<(), RunError> {
            assert_eq!(scope, "run:alice");
            if self.held.replace(true) {
                return Err(RunError::failure("coord.lease_held", "held"));
            }
            self.claims += 1;
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
        fn renew_held_checked(
            &mut self,
            scope: &str,
            budget: Duration,
        ) -> Result<u64, crate::mail::Renewal> {
            self.budgets.push(budget);
            std::thread::sleep(self.slow);
            let fail = |timed_out| crate::mail::Renewal {
                error: RunError::failure("run.mail_failed", "turn lease check timed out"),
                timed_out,
            };
            if self.timeouts > 0 {
                self.timeouts -= 1;
                return Err(fail(true));
            }
            if self.refusals > 0 {
                self.refusals -= 1;
                return Err(fail(false));
            }
            self.renew_held(scope)
                .map_err(|error| crate::mail::Renewal {
                    error,
                    timed_out: false,
                })
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

    /// Answers `sender_of` from a table, and counts the lookups; serves
    /// `unread` as header pages.
    #[derive(Default)]
    struct Senders {
        known: Vec<(&'static str, &'static str)>,
        asked: Vec<String>,
        unread: Vec<crate::mail::Header>,
        pages: usize,
    }
    impl Mail for Senders {
        fn headers(
            &mut self,
            _: &str,
            after: Option<u64>,
            limit: usize,
        ) -> Result<crate::mail::Headers, RunError> {
            self.pages += 1;
            let newer: Vec<_> = self
                .unread
                .iter()
                .filter(|h| h.seq > after.unwrap_or(0))
                .cloned()
                .collect();
            let messages: Vec<_> = newer.iter().take(limit.min(5)).cloned().collect();
            Ok(crate::mail::Headers {
                cursor: messages.iter().map(|h| h.seq).max(),
                more: newer.len() > messages.len(),
                messages,
            })
        }
        fn sender_of(&mut self, agent: &str, id: &str) -> Result<Option<String>, RunError> {
            assert_eq!(agent, "alice");
            self.asked.push(id.into());
            if id == "broken" {
                return Err(RunError::failure("run.mail_failed", "unreadable"));
            }
            Ok(self
                .known
                .iter()
                .find(|(m, _)| *m == id)
                .map(|(_, s)| s.to_string()))
        }
        fn claim(&mut self, _: &str) -> Result<(), RunError> {
            panic!("no claims")
        }
        fn release(&mut self, _: &str) -> Result<(), RunError> {
            panic!("no releases")
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
            panic!("no releases")
        }
        fn holds(&mut self, _: &str, _: &str, _: Option<&str>) -> Result<bool, RunError> {
            panic!("no file claims")
        }
        fn note(&mut self, _: &Note<'_>) -> Result<String, RunError> {
            panic!("no notes")
        }
    }

    #[test]
    fn a_note_replying_to_the_agents_own_message_never_wakes_the_receiver() {
        let page = |kind: &str, refs: &[&str]| crate::mail::Header {
            seq: 7,
            kind: kind.into(),
            sender: "orchestrator".into(),
            room: None,
            reference_ids: refs.iter().map(|r| r.to_string()).collect(),
        };
        let mut mail = Senders {
            known: vec![("mine", "alice"), ("theirs", "orchestrator")],
            ..Senders::default()
        };
        let got = wake_headers(
            &mut mail,
            "alice",
            vec![
                page("note", &["mine"]),
                page("note", &["theirs"]),
                page("note", &["unknown"]),
                page("note", &["broken"]),
                page("note", &[]),
                page("question", &["mine"]),
            ],
        );
        let woke: Vec<bool> = got.iter().map(hooks::wakes).collect();
        assert_eq!(woke, [false, true, true, true, true, true]);
        // Only a note that replies is looked up.
        assert_eq!(mail.asked, ["mine", "theirs", "unknown", "broken"]);
        assert!(reserve_payload(Path::new("/unused"), "alice", &Uuid::nil(), &got[..1]).is_none());
    }

    #[test]
    fn passive_notices_never_become_the_receiver_cap_fallback_or_hide_questions() {
        let count = (WAKE_PAGES + 1) * 5;
        let mut unread: Vec<_> = (1..=count as u64)
            .map(|seq| crate::mail::Header {
                seq,
                kind: "notice".into(),
                sender: "peer".into(),
                room: None,
                reference_ids: vec![],
            })
            .collect();
        let mut mail = Senders {
            unread: unread.clone(),
            ..Senders::default()
        };
        assert!(wake_page(&mut mail, "alice", 0, 200).unwrap().is_empty());
        unread.push(crate::mail::Header {
            seq: count as u64 + 1,
            kind: "question".into(),
            sender: "peer".into(),
            room: None,
            reference_ids: vec![],
        });
        mail.unread = unread;
        let got = wake_page(&mut mail, "alice", 0, 200).unwrap();
        assert_eq!(
            got.iter()
                .filter(|h| hooks::wakes(h))
                .map(|h| h.seq)
                .collect::<Vec<_>>(),
            [count as u64 + 1]
        );
    }

    #[test]
    fn wake_kinds_receiver_scans_past_excluded_pages_without_a_false_wake() {
        let excluded = 5 * (WAKE_PAGES + 1) as u64;
        let mut unread: Vec<_> = (1..=excluded)
            .map(|seq| crate::mail::Header {
                seq,
                kind: "note".into(),
                sender: "peer".into(),
                room: None,
                reference_ids: vec![],
            })
            .collect();
        unread.push(crate::mail::Header {
            seq: excluded + 1,
            kind: "question".into(),
            sender: "peer".into(),
            room: None,
            reference_ids: vec![],
        });
        let mut mail = Senders {
            unread,
            ..Senders::default()
        };
        let questions = vec!["question".into()];
        let got = wake_page_for(&mut mail, "alice", 0, 200, Some(&questions)).unwrap();
        assert_eq!(
            got.iter().map(|h| h.seq).collect::<Vec<_>>(),
            [excluded + 1]
        );
        assert!(mail.pages > WAKE_PAGES);
        assert_eq!(mail.unread.len(), (excluded + 1) as usize);
        mail.unread.pop();
        let got = wake_page_for(&mut mail, "alice", 0, 200, Some(&questions)).unwrap();
        assert!(got.is_empty(), "excluded notes never fail open");
        let pages = mail.pages;
        assert!(
            wake_page_for(&mut mail, "alice", 0, 200, Some(&[]))
                .unwrap()
                .is_empty()
        );
        assert_eq!(mail.pages, pages, "an empty filter reads no pages");
    }

    #[test]
    fn notes_that_dont_wake_never_hide_a_later_wake_behind_a_page() {
        let header = |seq: u64, kind: &str, refs: &[&str]| crate::mail::Header {
            seq,
            kind: kind.into(),
            sender: "peer".into(),
            room: None,
            reference_ids: refs.iter().map(|r| r.to_string()).collect(),
        };
        // Twelve acknowledgements of the agent's own handoff, then a
        // question: more than two pages of notes that never wake.
        let mut unread: Vec<_> = (1..=12)
            .map(|n| header(n, "note", &["paged-mine"]))
            .collect();
        unread.push(header(13, "question", &[]));
        unread.push(header(14, "note", &["paged-mine"]));
        let mut mail = Senders {
            known: vec![("paged-mine", "alice")],
            unread,
            ..Senders::default()
        };
        let got = wake_page(&mut mail, "alice", 0, 200).expect("pages");
        assert_eq!(mail.pages, 3, "paged until the question");
        let woke: Vec<u64> = got
            .iter()
            .filter(|h| hooks::wakes(h))
            .map(|h| h.seq)
            .collect();
        assert_eq!(woke, [13]);
        // The sender is looked up once, however many notes name it.
        assert!(mail.asked.len() <= 1, "{:?}", mail.asked);
        // From a floor past the question, nothing wakes, and paging stops
        // at the last page.
        let mut quiet = Senders {
            known: vec![("paged-mine", "alice")],
            unread: (1..=7)
                .map(|n| header(n, "note", &["paged-mine"]))
                .collect(),
            ..Senders::default()
        };
        let got = wake_page(&mut quiet, "alice", 0, 200).expect("pages");
        assert_eq!((got.len(), quiet.pages), (7, 2));
        assert!(!got.iter().any(hooks::wakes));
        // More notes than the pages it reads, then a question: it fails
        // open on the newest note read, so the floor moves past them.
        let many = WAKE_PAGES as u64 * 5 + 3;
        let mut unread: Vec<_> = (1..=many)
            .map(|n| header(n, "note", &["paged-mine"]))
            .collect();
        unread.push(header(many + 1, "question", &[]));
        let mut flooded = Senders {
            known: vec![("paged-mine", "alice")],
            unread,
            ..Senders::default()
        };
        let got = wake_page(&mut flooded, "alice", 0, 200).expect("pages");
        assert_eq!(flooded.pages, WAKE_PAGES);
        let woke: Vec<u64> = got
            .iter()
            .filter(|h| hooks::wakes(h))
            .map(|h| h.seq)
            .collect();
        assert_eq!(woke, [WAKE_PAGES as u64 * 5]);
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
    fn receiver_mode_uses_the_graph_ref_not_checkout_contents_or_head() {
        let f = Fixture::new();
        f.config(r#"{"agents":{"alice":{}}}"#);
        Fixture::git(&f.0, &["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
        Fixture::git(
            &f.0,
            &[
                "config",
                "--local",
                "mdkg.graph.ref",
                "refs/heads/mdkg-graph",
            ],
        );
        Fixture::git(
            &f.0,
            &[
                "config",
                "--local",
                "mdkg.graph.0192f3a0-0000-7000-8000-000000000000.ref",
                "refs/heads/mdkg-graph",
            ],
        );
        f.config(r#"{"agents":{"bob":{}}}"#);
        fs::write(f.0.join(".mdkg/config.json"), r#"{"agents":{"carol":{}}}"#).unwrap();

        assert_eq!(mode(&f.0, &f.dir(), "alice").unwrap(), Mode::Headless);
        assert_eq!(mode(&f.0, &f.dir(), "bob").unwrap(), Mode::Interactive);
        assert_eq!(mode(&f.0, &f.dir(), "carol").unwrap(), Mode::Interactive);
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
        let root = f.0.clone();
        let mut mail = LeaseMail {
            after_claim: Some(Box::new(move || {
                Fixture::config_at(&root, r#"{"agents":{"alice":{}}}"#);
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

    /// Room for several quick tries, however slow the machine.
    const QUICK: Backoff = Backoff {
        budget: Duration::from_secs(7),
        first: Duration::from_millis(1),
        most: Duration::from_millis(4),
        total: Duration::from_secs(10),
    };

    /// Room for pauses of 1 + 2 + 4 + 4 + 4 + 4 ms, and no seventh.
    const SHORT: Backoff = Backoff {
        total: Duration::from_millis(20),
        ..QUICK
    };

    fn start<T>(
        f: &Fixture,
        mail: &mut LeaseMail,
        retry: Option<Backoff>,
        body: impl FnOnce(Mode, &mut dyn Mail) -> Result<T, RunError>,
    ) -> Result<T, RunError> {
        with_mode_inner(mail, (&f.0, &f.dir(), "alice", &f.0), retry, Err, body)
    }

    /// friction 87: under load a start's lease check timed out and the
    /// supervisor exited. A start tries again, each time on a lease taken
    /// anew, until its allowance is spent; the callback runs once.
    #[test]
    fn a_start_retries_a_timed_out_lease_check_with_the_lease_released_between() {
        let f = Fixture::new();
        let mut mail = LeaseMail {
            timeouts: 3,
            ..LeaseMail::default()
        };
        let mut ran = 0;
        start(&f, &mut mail, Some(QUICK), |_, _| {
            ran += 1;
            Ok(())
        })
        .expect("started");
        assert_eq!((ran, mail.claims, mail.releases), (1, 4, 4));
        assert!(!mail.held.get());
        // Every try gets the start's budget, not a dispatcher's.
        assert_eq!(mail.budgets, [QUICK.budget; 4]);
        assert_eq!(START.budget, crate::mail::RECEIVER_START_BUDGET);
        assert_eq!(START.first, Duration::from_secs(1));
        assert_eq!(START.most, Duration::from_secs(15));
        assert_eq!(START.total, Duration::from_secs(120));
    }

    #[test]
    fn a_start_gives_up_when_its_allowance_is_spent_and_never_retries_an_answer() {
        let f = Fixture::new();
        // Pauses of 1 + 2 + 4 + 4 + 4 + 4 ms fit in 20, a seventh wouldn't,
        // and the tries' own time counts too: at most seven tries.
        let mut mail = LeaseMail {
            timeouts: 100,
            ..LeaseMail::default()
        };
        let e = start(&f, &mut mail, Some(SHORT), |_, _| -> Result<(), _> {
            panic!("never admitted")
        })
        .expect_err("gave up");
        assert!(e.message.contains("timed out"), "{e:?}");
        assert!((1..=7).contains(&mail.claims), "{}", mail.claims);
        assert_eq!(mail.claims, mail.releases);
        assert!(!mail.held.get());
        // Tries of 15 ms each: the second ends past 20 ms, so no third,
        // though the pauses alone would allow seven (review of a0b79a38).
        let mut mail = LeaseMail {
            timeouts: 100,
            slow: Duration::from_millis(15),
            ..LeaseMail::default()
        };
        start(&f, &mut mail, Some(SHORT), |_, _| -> Result<(), _> {
            panic!("never admitted")
        })
        .expect_err("gave up");
        assert!(mail.claims <= 2, "{}", mail.claims);
        assert_eq!(mail.claims, mail.releases);
        // A renewal that was answered (lost, refused) isn't tried again.
        let mut mail = LeaseMail {
            refusals: 1,
            ..LeaseMail::default()
        };
        start(&f, &mut mail, Some(QUICK), |_, _| -> Result<(), _> {
            panic!("never admitted")
        })
        .expect_err("refused");
        assert_eq!(mail.claims, 1);
        // Nor is any renewal without a retry allowance: a dispatcher's.
        let mut mail = LeaseMail {
            timeouts: 1,
            ..LeaseMail::default()
        };
        start(&f, &mut mail, None, |_, _| -> Result<(), _> {
            panic!("never admitted")
        })
        .expect_err("no retry");
        assert_eq!((mail.claims, mail.releases), (1, 1));
        // And it keeps a dispatcher's tight budget.
        assert_eq!(mail.budgets, [crate::mail::RECEIVER_RENEWAL_BUDGET]);
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
            own_reply: false,
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
    fn wake_kinds_reservation_accepts_selected_channel_notes_and_no_other_kinds() {
        let f = Fixture::new();
        let session = Uuid::now_v7();
        let selected = vec!["note".into()];
        let mut note = header(7);
        note.room = Some("dev".into());
        let mut question = header(8);
        question.kind = "question".into();
        let batch =
            reserve_payload_for(&f.0, "alice", &session, &[note, question], Some(&selected))
                .unwrap();
        assert_eq!(batch.through, 7);
        assert!(batch.line.contains("#7"));
        assert!(!batch.line.contains("#8"));
        assert!(
            reserve_payload_for(&f.0, "alice", &Uuid::now_v7(), &[header(9)], Some(&[])).is_none()
        );
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
                .contains("#7 review_request from a sender in channel release")
        );
        assert!(!batch.line.contains("INJECT"));
        assert!(!batch.line.contains("#99"));
        assert!(batch.line.contains("senders' data"));
    }
}
