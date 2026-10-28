//! Receiver-owned queue dispatch. A launched process with an uncertain result
//! consumes its reservation; only a definite spawn failure can roll it back.
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Mode, invalid, reserve_payload, route, with_mode_on_renewal_failure};
use crate::{
    Mail, RunError, hooks, identity,
    policy::Policy,
    state,
    supervise::{Event, Guard, Supervised, Up},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WakeStatus {
    Waiting,
    Busy,
    Suspended,
    Paused,
    NoMail,
    Queued,
    NotSent,
    DeliveryUnknown,
    ModeChanged,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Report {
    pub status: WakeStatus,
    pub session: Option<String>,
    pub through: Option<u64>,
}
impl Report {
    fn new(status: WakeStatus) -> Self {
        Self {
            status,
            session: None,
            through: None,
        }
    }
    fn launched(&self) -> bool {
        matches!(
            self.status,
            WakeStatus::Queued | WakeStatus::DeliveryUnknown
        )
    }
}

// Keep signal traps around the native absolute-clock guard. The shell owns
// the private group; killing only the native guard must also stop Codex,
// even while the receiver is stopped. No relative countdown starts here.
// Paths and mail are positional arguments, never shell source.
fn shell_guard() -> String {
    #[cfg(not(test))]
    let launch = "\"$@\" \"$$\" & child=$!";
    #[cfg(test)]
    let launch = "MDKG_QUEUE_GUARD_TEST_OWNER=$$ \"$@\" & child=$!";
    format!(
        r#"parent=$1; sleeper=$2; shift 2
stop() {{ trap '' INT TERM HUP; kill -s KILL -- "-$$" 2>/dev/null; exit 1; }}
trap stop INT TERM HUP
kill -0 "$parent" 2>/dev/null || exit 125
{launch}
while kill -0 "$child" 2>/dev/null; do
    kill -0 "$parent" 2>/dev/null || stop
    "$sleeper" 0.01 || stop
done
wait "$child"; result=$?
[ "$result" -eq 0 ] || [ "$result" -eq 125 ] || stop
exit "$result"
"#
    )
}

fn executable(program: &Path, cwd: &Path) -> Option<PathBuf> {
    executable_in(program, cwd, &std::env::var_os("PATH").unwrap_or_default())
}
fn executable_in(program: &Path, cwd: &Path, search_path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let candidates = if program.components().count() > 1 || program.is_absolute() {
        vec![cwd.join(program)]
    } else {
        std::env::split_paths(search_path)
            .filter(|p| p.is_absolute())
            .map(|p| p.join(program))
            .collect()
    };
    candidates.into_iter().find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

// Executable discovery and command construction precede the last ownership
// check. The lease expiry and policy cap remain absolute across every spawn.
struct QueueCommand {
    command: Command,
    spec: super::guard::Spec,
}
struct QueueChild {
    process: Child,
    deadline: u64,
}
impl QueueCommand {
    fn prepare(
        program: &Path,
        cwd: &Path,
        session: &str,
        line: &str,
        timeout: Duration,
    ) -> Result<Self, route::Delivery> {
        let deadline =
            state::now_ms().saturating_add(timeout.min(Duration::from_secs(30)).as_millis() as u64);
        let program = executable(program, cwd).ok_or(route::Delivery::NotSent)?;
        let guard = std::env::current_exe().map_err(|_| route::Delivery::NotSent)?;
        let sleep = crate::turn::find_system_tool("sleep").ok_or(route::Delivery::NotSent)?;
        let mut command = Command::new("/bin/sh");
        command
            .args(["-p", "-c", &shell_guard(), "mdkg-queue-guard"])
            .arg(std::process::id().to_string())
            .arg(sleep)
            .arg(guard);
        #[cfg(not(test))]
        command.arg("__receiver-queue-guard");
        #[cfg(test)]
        command.args(["--exact", super::guard::TEST_ENTRY, "--nocapture"]);
        command
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        for name in crate::profile::NOT_INHERITED
            .iter()
            .copied()
            .chain([crate::profile::TURN_MARK, "MDKG_ACTOR"])
        {
            command.env_remove(name);
        }
        for name in crate::auth::withheld_vars() {
            command.env_remove(name);
        }
        Ok(Self {
            command,
            spec: super::guard::Spec {
                deadline,
                program,
                session: session.into(),
                line: line.into(),
            },
        })
    }

    fn spawn_before(mut self, lease_deadline: u64) -> Result<QueueChild, route::Delivery> {
        self.spec.deadline = self.spec.deadline.min(lease_deadline);
        let spec = serde_json::to_string(&self.spec).map_err(|_| route::Delivery::NotSent)?;
        #[cfg(not(test))]
        self.command.arg(spec);
        #[cfg(test)]
        self.command.env(super::guard::TEST_SPEC, spec);
        if state::now_ms() >= self.spec.deadline {
            return Err(route::Delivery::NotSent);
        }
        let process = self.command.spawn().map_err(|_| route::Delivery::NotSent)?;
        Ok(QueueChild {
            process,
            deadline: self.spec.deadline,
        })
    }
}

#[cfg(test)]
fn spawn_queue_before(
    program: &Path,
    cwd: &Path,
    session: &str,
    line: &str,
    timeout: Duration,
    deadline: u64,
) -> Result<QueueChild, route::Delivery> {
    QueueCommand::prepare(program, cwd, session, line, timeout)?.spawn_before(deadline)
}

fn wait_queue_held(
    child: QueueChild,
    renew: impl FnMut() -> Result<(), RunError>,
) -> route::Delivery {
    wait_queue_held_at(child, renew, state::now_ms)
}

fn wait_queue_held_at(
    mut child: QueueChild,
    mut renew: impl FnMut() -> Result<(), RunError>,
    now: impl Fn() -> u64,
) -> route::Delivery {
    let mut renewed = Instant::now();
    loop {
        // Loss/outage after launch is ambiguous: kill the process group and
        // consume the reservation. Never restart a possibly delivered wake.
        let owns = now() < child.deadline
            && (renewed.elapsed() < Duration::from_millis(100) || {
                renewed = Instant::now();
                renew().is_ok()
            });
        match child.process.try_wait() {
            Ok(Some(status)) if status.code() == Some(super::guard::NOT_SENT.into()) => {
                // The guard reserves this exit code for never starting Codex.
                return route::Delivery::NotSent;
            }
            Ok(Some(status)) if owns && now() < child.deadline => {
                if status.success() {
                    return route::Delivery::Queued;
                }
                // A terminated outer guard can still have live descendants.
                stop_queue(&mut child);
                return route::Delivery::DeliveryUnknown;
            }
            Ok(None) if owns && now() < child.deadline => std::thread::sleep(super::guard::TICK),
            _ => {
                stop_queue(&mut child);
                return route::Delivery::DeliveryUnknown;
            }
        }
    }
}

fn stop_queue(child: &mut QueueChild) {
    if let Some(pid) = rustix::process::Pid::from_raw(child.process.id() as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.process.kill();
    let _ = child.process.wait();
}

/// Reconcile under both locks, then check lock-free markers after all other
/// admission reads. This final check admits the spawn; a later observation
/// does not retract it. The callback must not wait for the child or call hooks.
#[cfg(test)]
fn spawn_current<T>(
    common: &Path,
    actor: &str,
    snapshot: &route::Route,
    spawn: impl FnOnce() -> T,
) -> Result<Result<T, WakeStatus>, RunError> {
    spawn_current_held(common, actor, snapshot, || Ok(u64::MAX), |_| spawn())
}

fn spawn_current_held<T>(
    common: &Path,
    actor: &str,
    snapshot: &route::Route,
    renew: impl FnOnce() -> Result<u64, RunError>,
    spawn: impl FnOnce(u64) -> T,
) -> Result<Result<T, WakeStatus>, RunError> {
    route::with_current(common, actor, |r| {
        if r.observation_pending || r.suspended {
            return Ok(Err(WakeStatus::Suspended));
        }
        if r.generation != snapshot.generation
            || r.session != snapshot.session
            || r.cwd != snapshot.cwd
        {
            return Ok(Err(WakeStatus::Waiting));
        }
        if r.working {
            return Ok(Err(WakeStatus::Busy));
        }
        let cwd = r
            .cwd
            .as_deref()
            .ok_or_else(|| invalid("receiver folder is missing"))?;
        if !crate::projection::Projections::read(cwd)?.hooks(crate::projection::Harness::Codex) {
            return Ok(Err(WakeStatus::Waiting));
        }
        // Locks may have waited past expiry or a takeover. Renew without
        // reclaiming before the final marker/admission check.
        let deadline = renew()?;
        if super::evidence::pending(common, actor)? {
            return Ok(Err(WakeStatus::Suspended));
        }
        Ok(Ok(spawn(deadline)))
    })
}

/// One bounded admission and delivery attempt, wholly under the actor's run
/// lease. Header reads are no-mark. Final route validation and process creation
/// share the record lock; it is released before waiting, so hooks cannot deadlock.
pub(super) fn step(
    mail: &mut dyn Mail,
    root: &Path,
    common: &Path,
    dir: &Path,
    actor: &str,
    policy: &Policy,
    program: &Path,
) -> Result<Report, RunError> {
    let not_sent = |_| Ok(Report::new(WakeStatus::NotSent));
    with_mode_on_renewal_failure(mail, root, dir, actor, root, not_sent, |mode, mail| {
        if mode != Mode::Interactive {
            return Ok(Report::new(WakeStatus::ModeChanged));
        }
        let Some(snapshot) = route::status(common, actor)? else {
            return Ok(Report::new(WakeStatus::Waiting));
        };
        if snapshot.suspended || snapshot.observation_pending {
            return Ok(Report::new(WakeStatus::Suspended));
        }
        let (Some(session), Some(cwd)) = (&snapshot.session, &snapshot.cwd) else {
            return Ok(Report::new(WakeStatus::Waiting));
        };
        if !crate::projection::Projections::read(cwd)?.hooks(crate::projection::Harness::Codex) {
            return Ok(Report::new(WakeStatus::Waiting));
        }
        if snapshot.working {
            return Ok(Report::new(WakeStatus::Busy));
        }
        let trust = mdkg_coord_sqlite::git::WorktreeTrust {
            common: Some(common.to_path_buf()),
            worktree: None,
        };
        let actor_here = mdkg_coord_sqlite::git::with_worktree_trust(&trust, || {
            identity::actor_for_cwd(root, cwd)
        })?;
        if actor_here.as_deref() != Some(actor) {
            return Err(invalid("receiver worktree no longer identifies this actor"));
        }
        state::not_installing(dir, cwd)?;
        let scope = crate::turn::lease_scope(actor);
        if mail.renew_held(&scope).is_err() {
            return Ok(Report::new(WakeStatus::NotSent));
        }
        let floor = hooks::try_reminded_through(common, actor, session)
            .map_err(|_| invalid("receiver reminder record is unreadable"))?;
        let page = mail.headers(actor, Some(floor), policy.batch_max().min(200))?;
        let headers: Vec<_> = page
            .messages
            .into_iter()
            .map(|h| hooks::Mail {
                seq: h.seq,
                kind: h.kind,
                sender: h.sender,
                room: h.room,
            })
            .collect();
        let session_id =
            Uuid::parse_str(session).map_err(|_| invalid("receiver session isn't a UUID"))?;
        let attempt_id = Uuid::now_v7().to_string();
        let mut reserved = None;
        let mut renewal_failed = false;
        let prepared = route::update(common, actor, |r| {
            if r.suspended || r.observation_pending || super::evidence::pending(common, actor)? {
                return Ok(Report::new(WakeStatus::Suspended));
            }
            if r.generation != snapshot.generation
                || r.session != snapshot.session
                || r.cwd != snapshot.cwd
            {
                return Ok(Report::new(WakeStatus::Waiting));
            }
            if r.working {
                return Ok(Report::new(WakeStatus::Busy));
            }
            let key = format!("codex:{session}");
            let mut result = Report::new(WakeStatus::NoMail);
            Guard::update(dir, actor, false, |guard| {
                mail.renew_held(&scope).inspect_err(|_| {
                    renewal_failed = true;
                })?;
                let now = state::now_ms();
                if policy
                    .loop_guard
                    .is_some_and(|g| guard.paused(&g, now).contains(&key))
                {
                    result.status = WakeStatus::Paused;
                    return Ok(());
                }
                reserved = reserve_payload(common, actor, &session_id, &headers);
                if let Some(batch) = &reserved {
                    if let Some(g) = &policy.loop_guard {
                        guard.record(g, &key, now);
                    }
                    r.last_attempt = Some(route::Attempt {
                        id: attempt_id.clone(),
                        session: session.clone(),
                        through: batch.through,
                        at_ms: now,
                        delivery: route::Delivery::DeliveryUnknown,
                    });
                    result = Report {
                        status: WakeStatus::DeliveryUnknown,
                        session: Some(session.clone()),
                        through: Some(batch.through),
                    };
                }
                Ok(())
            })?;
            Ok(result)
        });
        let report = match prepared {
            Ok(report) => report,
            Err(e) => {
                if let Some(batch) = reserved {
                    hooks::unclaim_reminder(common, actor, session, batch.through, batch.previous);
                }
                if renewal_failed {
                    return Ok(Report::new(WakeStatus::NotSent));
                }
                return Err(e);
            }
        };
        let Some(batch) = reserved else {
            return Ok(report);
        };
        let timeout = policy
            .timeout
            .unwrap_or(Duration::from_secs(30))
            .min(Duration::from_secs(30));
        // Lookup may block (PATH/filesystem/executable loading). Finish it
        // before renewing ownership for the final admission check.
        let command = QueueCommand::prepare(program, cwd, session, &batch.line, timeout);
        let mut renewal_failed = false;
        let started = spawn_current_held(
            common,
            actor,
            &snapshot,
            || {
                if super::mode(root, dir, actor)? != Mode::Interactive {
                    return Err(invalid("receiver actor mode changed before dispatch"));
                }
                // Recheck after every preparation wait. Install publishes its
                // marker before enumerating registrations and live run leases,
                // then holds this same lease through its writes and undo.
                state::not_installing(dir, cwd)?;
                mail.renew_held(&scope).inspect_err(|_| {
                    renewal_failed = true;
                })
            },
            |deadline| command.and_then(|c| c.spawn_before(deadline)),
        );
        let (delivery, refused) = match started {
            Ok(Ok(Ok(child))) => (
                wait_queue_held(child, || mail.renew_held(&scope).map(|_| ())),
                None,
            ),
            Ok(Ok(Err(delivery))) => (delivery, None),
            Ok(Err(status)) => (route::Delivery::NotSent, Some(status)),
            // Renewal refusal before process creation is definite non-delivery.
            // Let the common completion path record NotSent and unclaim once.
            Err(_) if renewal_failed => (route::Delivery::NotSent, None),
            Err(e) => {
                // No process was created; a corrupt/interrupted observation
                // must not consume the stop hook's reminder reservation.
                hooks::unclaim_reminder(common, actor, session, batch.through, batch.previous);
                return Err(e);
            }
        };
        if delivery == route::Delivery::NotSent {
            hooks::unclaim_reminder(common, actor, session, batch.through, batch.previous);
        }
        route::update(common, actor, |r| {
            if let Some(attempt) = &mut r.last_attempt
                && attempt.id == attempt_id
            {
                attempt.delivery = delivery;
            }
            Ok(())
        })?;
        Ok(Report {
            status: refused.unwrap_or(match delivery {
                route::Delivery::Queued => WakeStatus::Queued,
                route::Delivery::NotSent => WakeStatus::NotSent,
                route::Delivery::DeliveryUnknown => WakeStatus::DeliveryUnknown,
            }),
            ..report
        })
    })
}

/// Bounded header waits plus state rechecks; no model polling or body listing.
/// A mode change stops this watcher and requires restarting agents up.
pub fn supervise(
    mail: &mut dyn Mail,
    dir: &Path,
    a: &Supervised,
    how: &Up,
    on: &(dyn Fn(&str, Event<'_>) + Sync),
) -> u64 {
    let Some(root) = a.opts.graph_root.as_deref() else {
        return 0;
    };
    let Some(common) = a.opts.common_dir.as_deref() else {
        return 0;
    };
    let program: PathBuf = a
        .opts
        .program
        .clone()
        .unwrap_or_else(|| crate::Harness::Codex.program());
    let mut ran = 0;
    let mut seen = None;
    let mut last = None;
    loop {
        if how.max_turns.is_some_and(|n| ran >= n) {
            return ran;
        }
        match step(mail, root, common, dir, &a.name, &a.policy, &program) {
            Ok(report) => {
                if last.as_ref() != Some(&report) {
                    on(&a.name, Event::Wake(&report));
                }
                if report.launched() {
                    ran += 1;
                }
                if report.status == WakeStatus::ModeChanged || how.once {
                    return ran;
                }
                if report.status == WakeStatus::NotSent {
                    std::thread::sleep(how.pause);
                }
                last = Some(report);
            }
            Err(e) => {
                on(&a.name, Event::Trouble(&e));
                if how.once {
                    return ran;
                }
                std::thread::sleep(how.pause);
            }
        }
        match mail.wait_headers(&a.name, seen, how.wait_secs.clamp(1, 5)) {
            Ok(Some(page)) => seen = seen.max(page.cursor),
            Ok(None) => (),
            Err(e) => {
                on(&a.name, Event::Trouble(&e));
                std::thread::sleep(how.pause);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    // Only fixture readiness/handoff waits use this; expiry assertions below
    // retain their own short deadlines.
    const STARTUP_ALLOWANCE: Duration = Duration::from_secs(30);
    const QUEUE_WATCHDOG: Duration = Duration::from_secs(60);

    fn check_queue_watchdog(started: Instant, budget: Duration) {
        assert!(
            started.elapsed() < budget,
            "test queue watchdog expired after {budget:?}"
        );
    }

    // Only test builds install this unwind cleanup. A normal return must not
    // leave a running child. Keep one owner/reaper; never signal a cached PID
    // from another thread.
    impl Drop for QueueChild {
        fn drop(&mut self) {
            if matches!(self.process.try_wait(), Ok(None)) {
                let unwinding = std::thread::panicking();
                stop_queue(self);
                assert!(
                    unwinding,
                    "fixture left a running child; production did not stop the queue"
                );
            }
        }
    }

    fn wait_success(mut child: QueueChild, budget: Duration) -> QueueChild {
        let started = Instant::now();
        loop {
            check_queue_watchdog(started, budget);
            if let Some(status) = child.process.try_wait().expect("fixture child status") {
                assert!(status.success(), "fixture child failed: {status}");
                return child;
            }
            std::thread::sleep(super::super::guard::TICK);
        }
    }

    fn wait_exited(pid: u32) {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        let pid = Pid::from_raw(pid as i32).unwrap();
        let start = Instant::now();
        while waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        )
        .unwrap()
        .is_none()
        {
            assert!(
                start.elapsed() < STARTUP_ALLOWANCE,
                "guard did not stop the child"
            );
            std::thread::sleep(super::super::guard::TICK);
        }
    }

    fn wait_pid_gone(pid: i32) {
        use rustix::process::{Pid, test_kill_process};
        let pid = Pid::from_raw(pid).unwrap();
        let start = Instant::now();
        loop {
            match test_kill_process(pid) {
                Err(rustix::io::Errno::SRCH) => return,
                Ok(()) => {}
                Err(error) => panic!("can't inspect fixture process: {error}"),
            }
            assert!(
                start.elapsed() < STARTUP_ALLOWANCE,
                "queue descendant survived cancellation"
            );
            std::thread::sleep(super::super::guard::TICK);
        }
    }

    fn wait_for_lock<T>(probe: &fs::File, worker: &std::thread::JoinHandle<T>) {
        let started = Instant::now();
        loop {
            match probe.try_lock() {
                Ok(()) => probe.unlock().unwrap(),
                Err(fs::TryLockError::WouldBlock) => return,
                Err(e) => panic!("{e}"),
            }
            assert!(
                !worker.is_finished(),
                "dispatcher exited before registry wait"
            );
            assert!(
                started.elapsed() < STARTUP_ALLOWANCE,
                "dispatcher did not reach registry wait"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    struct Fake(PathBuf);
    impl Fake {
        fn new(script: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("mdkg-queue-{}", Uuid::now_v7()));
            fs::create_dir_all(&dir).unwrap();
            let f = Self(dir);
            f.script(script);
            f.set_clock(0);
            f
        }
        fn script(&self, script: &str) {
            fs::write(
                self.0.join("fake"),
                format!("#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nprintf started > \"$0.started\"\n{script}\n"),
            )
            .unwrap();
            fs::set_permissions(self.0.join("fake"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        // Certainty tests have no expiry premise. Keep both processes' clocks
        // out of the verdict; dedicated tests advance expiry after readiness.
        fn prepare(&self) -> Result<QueueCommand, route::Delivery> {
            let mut c = QueueCommand::prepare(
                &self.0.join("fake"),
                &self.0,
                "thread",
                "header",
                STARTUP_ALLOWANCE,
            )?;
            c.spec.deadline = u64::MAX;
            c.command
                .env(super::super::guard::TEST_CLOCK, self.0.join("clock"));
            Ok(c)
        }
        fn spawn(&self) -> QueueChild {
            self.prepare().unwrap().spawn_before(u64::MAX).unwrap()
        }
        fn run(&self) -> route::Delivery {
            match self.prepare().and_then(|c| c.spawn_before(u64::MAX)) {
                Ok(child) => self.wait(child, || Ok(())),
                Err(delivery) => delivery,
            }
        }
        fn wait(
            &self,
            child: QueueChild,
            renew: impl FnMut() -> Result<(), RunError>,
        ) -> route::Delivery {
            self.wait_for(child, renew, QUEUE_WATCHDOG)
        }
        fn wait_for(
            &self,
            child: QueueChild,
            renew: impl FnMut() -> Result<(), RunError>,
            budget: Duration,
        ) -> route::Delivery {
            let started = Instant::now();
            wait_queue_held_at(child, renew, || {
                check_queue_watchdog(started, budget);
                self.clock()
            })
        }
        fn set_clock(&self, now: u64) {
            let tmp = self.0.join("next-clock");
            fs::write(&tmp, now.to_string()).unwrap();
            fs::rename(tmp, self.0.join("clock")).unwrap();
        }
        fn clock(&self) -> u64 {
            fs::read_to_string(self.0.join("clock"))
                .unwrap()
                .parse()
                .unwrap()
        }
        fn queue_pid(&self) -> i32 {
            fs::read_to_string(self.0.join("fake.pid"))
                .unwrap()
                .parse()
                .unwrap()
        }
        fn wait_started(&self, child: &mut QueueChild) {
            let start = Instant::now();
            while !self.0.join("fake.started").exists() {
                assert!(
                    child.process.try_wait().unwrap().is_none(),
                    "queue exited before readiness"
                );
                assert!(start.elapsed() < STARTUP_ALLOWANCE, "queue did not start");
                std::thread::sleep(super::super::guard::TICK);
            }
        }
    }
    #[test]
    fn normal_return_without_stopping_queue_is_reported_and_cleaned_up() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        // Mutation stand-in: return the same certainty as lease loss while
        // omitting the production stop. Fixture cleanup must not mask it.
        fn skip_stop(_child: QueueChild) -> route::Delivery {
            route::Delivery::DeliveryUnknown
        }
        let f = Fake::new("while :; do /bin/sleep 0.01; done");
        let mut child = f.spawn();
        f.wait_started(&mut child);
        let queue = f.queue_pid();
        let leader = rustix::process::Pid::from_raw(child.process.id() as i32).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| skip_stop(child)));
        wait_pid_gone(queue);
        assert_eq!(
            rustix::process::waitpid(Some(leader), rustix::process::WaitOptions::NOHANG)
                .expect_err("the leaked queue leader must still be reaped"),
            rustix::io::Errno::CHILD
        );
        let failure = result.expect_err("fixture cleanup must report the missing production stop");
        let message = failure
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| failure.downcast_ref::<&str>().copied())
            .expect("fixture failure message");
        assert!(
            message.contains("fixture left a running child; production did not stop the queue"),
            "{message}"
        );
        assert_eq!(f.clock(), 0, "cleanup must not advance simulated time");
    }

    #[test]
    fn frozen_clock_watchdog_cleans_a_hung_queue_for_both_wait_paths() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        for raw_wait in [false, true] {
            let f = Fake::new("while :; do /bin/sleep 0.01; done");
            let mut child = f.spawn();
            f.wait_started(&mut child);
            let queue = f.queue_pid();
            let leader = rustix::process::Pid::from_raw(child.process.id() as i32).unwrap();
            let failure = catch_unwind(AssertUnwindSafe(|| {
                if raw_wait {
                    let _ = wait_success(child, Duration::ZERO);
                } else {
                    let _ = f.wait_for(child, || Ok(()), Duration::ZERO);
                }
            }))
            .expect_err("the watchdog must fail a frozen-clock hang");
            let message = failure.downcast_ref::<String>().expect("watchdog message");
            assert!(message.contains("test queue watchdog expired"), "{message}");
            assert_eq!(f.clock(), 0, "watchdog must not advance simulated time");
            wait_pid_gone(queue);
            assert_eq!(
                rustix::process::waitpid(Some(leader), rustix::process::WaitOptions::NOHANG)
                    .expect_err("the watchdog must reap its direct child"),
                rustix::io::Errno::CHILD
            );
        }
    }
    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn spawn_and_exit_have_distinct_delivery_certainty() {
        let f = Fake::new("exit 0");
        assert_eq!(f.run(), route::Delivery::Queued);
        f.script("exit 1");
        assert_eq!(f.run(), route::Delivery::DeliveryUnknown);
        fs::remove_file(f.0.join("fake")).unwrap();
        assert_eq!(f.run(), route::Delivery::NotSent);
    }
    #[test]
    fn hung_queue_is_bounded_and_ambiguous() {
        let f = Fake::new("while :; do /bin/sleep 0.01; done");
        let mut child = f.spawn();
        f.wait_started(&mut child);
        let at = Instant::now();
        f.set_clock(u64::MAX);
        assert_eq!(f.wait(child, || Ok(())), route::Delivery::DeliveryUnknown);
        assert!(at.elapsed() < Duration::from_secs(3));
    }
    #[test]
    fn automatic_lookup_ignores_empty_dot_and_relative_path_entries() {
        let f = Fake::new("exit 0");
        fs::create_dir(f.0.join("bin")).unwrap();
        fs::copy(f.0.join("fake"), f.0.join("bin/fake")).unwrap();
        for path in ["", ".", "bin", ":.:bin:"] {
            assert!(executable_in(Path::new("fake"), &f.0, path.as_ref()).is_none());
        }
        let search = std::env::join_paths([Path::new("."), f.0.as_path()]).unwrap();
        assert_eq!(
            executable_in(Path::new("fake"), &f.0, &search),
            Some(f.0.join("fake"))
        );
        // A separately supplied explicit path is an operator override.
        assert_eq!(
            executable_in(Path::new("./fake"), &f.0, "".as_ref()),
            Some(f.0.join("./fake"))
        );
    }

    fn idle_route(f: &Fake) -> route::Route {
        for dir in [".git/objects", ".git/refs"] {
            fs::create_dir_all(f.0.join(dir)).unwrap();
        }
        fs::write(f.0.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            f.0.join(".git/config"),
            "[core]\nrepositoryformatversion = 0\nbare = false\n",
        )
        .unwrap();
        fs::create_dir_all(f.0.join(".mdkg")).unwrap();
        fs::write(
            f.0.join(".mdkg/config.json"),
            r#"{"projections":["hooks:codex"]}"#,
        )
        .unwrap();
        route::observe(
            &f.0,
            "alice",
            SESSION_A,
            &f.0,
            hooks::ThreadState::Idle,
            &observation(hooks::HarnessEvent::SessionStart),
        )
        .unwrap();
        route::read(&f.0, "alice").unwrap().unwrap()
    }

    const SESSION_A: &str = "019a1111-1111-7111-8111-111111111111";
    const SESSION_B: &str = "019a2222-2222-7222-8222-222222222222";

    fn observation(event: hooks::HarnessEvent) -> hooks::HookObservation {
        hooks::HookObservation {
            event: hooks::HookEvent::Start,
            harness_event: Some(event),
            at_ms: state::now_ms(),
            start_provenance: (event == hooks::HarnessEvent::SessionStart)
                .then(|| hooks::StartProvenance::parse(Some("startup"), None)),
        }
    }

    #[test]
    fn failed_write_before_truncation_never_queues_old_thread() {
        let f = Fake::new("printf queued > invoked");
        let snapshot = idle_route(&f);
        route::begin_with(
            &f.0,
            "alice",
            SESSION_B,
            &f.0,
            &observation(hooks::HarnessEvent::SessionStart),
            || Err(invalid("injected open failure before truncation")),
        )
        .unwrap_err();
        // Fault leaves exactly the prior eligible bytes. A subsequent write
        // succeeds; that alone must not restore dispatch authority.
        assert_eq!(
            route::read(&f.0, "alice").unwrap().unwrap().generation,
            snapshot.generation
        );
        route::update(&f.0, "alice", |_| Ok(())).unwrap();
        let invoked = std::cell::Cell::new(false);
        assert_eq!(
            spawn_current(&f.0, "alice", &snapshot, || invoked.set(true)).unwrap(),
            Err(WakeStatus::Suspended)
        );
        assert!(!invoked.get());
    }

    #[test]
    fn crash_before_truncation_restart_never_queues_old_thread() {
        const CHILD: &str = "MDKG_RECEIVER_CRASH_FIXTURE";
        if let Some(path) = std::env::var_os(CHILD) {
            let common = PathBuf::from(path);
            let _ = route::begin_with(
                &common,
                "alice",
                SESSION_B,
                &common,
                &observation(hooks::HarnessEvent::SessionStart),
                || std::process::exit(73),
            );
            panic!("crash seam was not reached");
        }
        for failed_write in ["none", "witness", "registry"] {
            let f = Fake::new("printf queued > invoked");
            let snapshot = idle_route(&f);
            let witnesses = f.0.join("mdkg/hooks/receiver/alice.observed");
            let registry_lock = f.0.join("mdkg/hooks/threads.lock");
            if failed_write == "witness" {
                fs::remove_dir(&witnesses).unwrap();
                fs::write(&witnesses, "injected thread witness create failure").unwrap();
            } else if failed_write == "registry" {
                fs::remove_file(&registry_lock).unwrap();
                fs::create_dir(&registry_lock).unwrap();
            }
            let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "receiver::dispatch::tests::crash_before_truncation_restart_never_queues_old_thread", "--nocapture"])
            .env(CHILD, &f.0).env_remove("MDKG_TRUSTED_COMMON_DIR").env_remove("MDKG_TRUSTED_WORKTREE")
            .stdout(Stdio::null()).status().unwrap();
            assert_eq!(status.code(), Some(73), "{failed_write}");
            assert_eq!(
                route::read(&f.0, "alice").unwrap().unwrap().generation,
                snapshot.generation
            );
            if failed_write == "witness" {
                fs::remove_file(&witnesses).unwrap();
            }
            if failed_write == "registry" {
                fs::remove_dir(&registry_lock).unwrap();
            }
            route::update(&f.0, "alice", |_| Ok(())).unwrap();
            // This process never observed B. Either independent durable write
            // may have failed before the crash; the other still refuses A.
            let invoked = std::cell::Cell::new(false);
            assert_eq!(
                spawn_current(&f.0, "alice", &snapshot, || invoked.set(true)).unwrap(),
                Err(WakeStatus::Suspended),
                "{failed_write}"
            );
            assert!(!invoked.get());
            // Normal pruning must not erase evidence after a failed
            // witness or registry write plus crash. Advance the fixture timestamp eight
            // days, then issue a late A Stop through the real observation API.
            let later = hooks::HookObservation {
                event: hooks::HookEvent::Stop,
                harness_event: Some(hooks::HarnessEvent::Stop),
                at_ms: state::now_ms() + 8 * 24 * 60 * 60 * 1000,
                start_provenance: None,
            };
            route::observe(
                &f.0,
                "alice",
                SESSION_A,
                &f.0,
                hooks::ThreadState::Idle,
                &later,
            )
            .unwrap();
            assert!(
                !f.0.join(format!("mdkg/hooks/threads/codex-{SESSION_B}.json"))
                    .exists()
            );
            let current = route::read(&f.0, "alice").unwrap().unwrap();
            assert_eq!(
                spawn_current(&f.0, "alice", &current, || invoked.set(true)).unwrap(),
                Err(WakeStatus::Suspended),
                "after pruning: {failed_write}"
            );
            assert!(!invoked.get());
        }
    }

    #[test]
    fn crash_after_marker_before_lock_never_queues_old_thread() {
        early_fault(
            "crash_after_marker_before_lock_never_queues_old_thread",
            false,
            true,
            false,
        );
    }

    #[test]
    fn lock_open_failure_never_queues_old_thread() {
        early_fault(
            "lock_open_failure_never_queues_old_thread",
            true,
            false,
            false,
        );
    }

    #[test]
    fn lock_open_failure_then_crash_never_queues_old_thread() {
        early_fault(
            "lock_open_failure_then_crash_never_queues_old_thread",
            true,
            true,
            false,
        );
    }

    #[test]
    fn first_registration_crash_after_marker_is_visible_and_resettable() {
        early_fault(
            "first_registration_crash_after_marker_is_visible_and_resettable",
            false,
            true,
            true,
        );
    }

    fn early_fault(test: &str, lock_failure: bool, crash: bool, first: bool) {
        const CHILD: &str = "MDKG_RECEIVER_EARLY_FAULT";
        if let Some(path) = std::env::var_os(CHILD) {
            let common = PathBuf::from(path);
            let result = route::begin_at(
                &common,
                "alice",
                SESSION_B,
                &common,
                &observation(hooks::HarnessEvent::SessionStart),
                || {
                    if crash && !lock_failure {
                        std::process::exit(74);
                    }
                    Ok(())
                },
                || panic!("route work must not be reached"),
            );
            assert!(result.unwrap_err().message.contains("lock receiver route"));
            if crash {
                std::process::exit(74);
            }
            return;
        }
        let f = Fake::new("exit 0");
        let snapshot = (!first).then(|| idle_route(&f));
        let lock = f.0.join("mdkg/hooks/receiver/alice.lock");
        if lock_failure {
            fs::remove_file(&lock).unwrap();
            fs::create_dir(&lock).unwrap();
        }
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("receiver::dispatch::tests::{test}"),
                "--nocapture",
            ])
            .env(CHILD, &f.0)
            .env_remove("MDKG_TRUSTED_COMMON_DIR")
            .env_remove("MDKG_TRUSTED_WORKTREE")
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(if crash { 74 } else { 0 }));
        if lock_failure {
            fs::remove_dir(&lock).unwrap();
        }
        // No registry or route write by B was needed to retain the latch.
        assert!(
            !f.0.join(format!("mdkg/hooks/threads/codex-{SESSION_B}.json"))
                .exists()
        );
        assert!(route::actors(&f.0).contains(&"alice".into()));
        let visible = route::status(&f.0, "alice").unwrap().unwrap();
        assert!(visible.observation_pending);
        if let Some(snapshot) = snapshot {
            assert_eq!(
                route::read(&f.0, "alice").unwrap().unwrap().generation,
                snapshot.generation
            );
            route::update(&f.0, "alice", |_| Ok(())).unwrap();
            let invoked = std::cell::Cell::new(false);
            assert_eq!(
                spawn_current(&f.0, "alice", &snapshot, || invoked.set(true)).unwrap(),
                Err(WakeStatus::Suspended)
            );
            assert!(!invoked.get());
        } else {
            assert!(route::read(&f.0, "alice").unwrap().is_none());
            assert!(visible.session.is_none());
            route::reset(&f.0, "alice").unwrap();
            assert_eq!(
                fs::read_dir(f.0.join("mdkg/hooks/receiver/alice.pending"))
                    .unwrap()
                    .count(),
                0
            );
            assert!(
                route::read(&f.0, "alice")
                    .unwrap()
                    .unwrap()
                    .observation_pending
            );
        }
    }

    #[test]
    fn corrupt_route_repeated_observations_keep_bounded_evidence() {
        let f = Fake::new("exit 0");
        idle_route(&f);
        let file = f.0.join("mdkg/hooks/receiver/alice.json");
        fs::write(&file, "{").unwrap();
        for _ in 0..200 {
            assert!(
                route::begin_observation(
                    &f.0,
                    "alice",
                    SESSION_B,
                    &f.0,
                    &observation(hooks::HarnessEvent::SessionStart)
                )
                .is_err()
            );
        }
        assert_eq!(
            fs::read_dir(f.0.join("mdkg/hooks/receiver/alice.pending"))
                .unwrap()
                .count(),
            65
        );
        assert_eq!(
            fs::read_dir(f.0.join("mdkg/hooks/receiver/alice.observed"))
                .unwrap()
                .count(),
            64
        );
        route::reset(&f.0, "alice").unwrap();
        assert_eq!(
            fs::read_dir(f.0.join("mdkg/hooks/receiver/alice.pending"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn overflow_survives_completion_and_stale_ticket_cannot_clear_reused_slot() {
        let f = Fake::new("exit 0");
        idle_route(&f);
        let obs = observation(hooks::HarnessEvent::UserPromptSubmit);
        let tickets: Vec<_> = (0..64)
            .map(|_| route::begin_observation(&f.0, "alice", SESSION_A, &f.0, &obs).unwrap())
            .collect();
        assert!(route::begin_observation(&f.0, "alice", SESSION_B, &f.0, &obs).is_err());
        for ticket in tickets {
            route::finish_observation(&f.0, "alice", ticket, hooks::ThreadState::Idle).unwrap();
        }
        assert!(
            route::status(&f.0, "alice")
                .unwrap()
                .unwrap()
                .observation_pending
        );
        route::reset(&f.0, "alice").unwrap();
        let old = route::begin_observation(&f.0, "alice", SESSION_A, &f.0, &obs).unwrap();
        route::reset(&f.0, "alice").unwrap();
        let new = route::begin_observation(&f.0, "alice", SESSION_A, &f.0, &obs).unwrap();
        assert!(route::finish_observation(&f.0, "alice", old, hooks::ThreadState::Idle).is_err());
        assert!(
            route::status(&f.0, "alice")
                .unwrap()
                .unwrap()
                .observation_pending
        );
        route::finish_observation(&f.0, "alice", new, hooks::ThreadState::Idle).unwrap();
        assert_eq!(
            fs::read_dir(f.0.join("mdkg/hooks/receiver/alice.pending"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn registry_changes_and_corruption_refuse_final_spawn() {
        let f = Fake::new("exit 0");
        let snapshot = idle_route(&f);
        let registry =
            f.0.join(format!("mdkg/hooks/threads/codex-{SESSION_A}.json"));
        let original = fs::read(&registry).unwrap();
        fs::write(&registry, "{").unwrap();
        let invoked = std::cell::Cell::new(false);
        assert!(spawn_current(&f.0, "alice", &snapshot, || invoked.set(true)).is_err());
        assert!(!invoked.get());
        fs::write(&registry, original).unwrap();
        // An independent working observation after preparation blocks even
        // when the route bytes/generation did not change.
        hooks::record_thread(
            &f.0,
            &hooks::Thread::observed(
                "alice".into(),
                "codex".into(),
                SESSION_A.into(),
                f.0.clone(),
                hooks::ThreadState::Working,
                observation(hooks::HarnessEvent::UserPromptSubmit),
            ),
        )
        .unwrap();
        assert!(matches!(
            spawn_current(&f.0, "alice", &snapshot, || invoked.set(true)).unwrap(),
            Err(WakeStatus::Suspended | WakeStatus::Busy)
        ));
        assert!(!invoked.get());
        let f = Fake::new("exit 0");
        let snapshot = idle_route(&f);
        // General registry-only evidence still matters: no pending marker or
        // retained witness is present in this independent-observer case.
        hooks::record_thread(
            &f.0,
            &hooks::Thread::observed(
                "alice".into(),
                "codex".into(),
                SESSION_B.into(),
                f.0.clone(),
                hooks::ThreadState::Working,
                observation(hooks::HarnessEvent::SessionStart),
            ),
        )
        .unwrap();
        assert_eq!(
            spawn_current(&f.0, "alice", &snapshot, || invoked.set(true)).unwrap(),
            Err(WakeStatus::Suspended)
        );
        assert!(!invoked.get());
    }

    #[test]
    fn switch_between_preparation_and_spawn_never_invokes_the_spawn_callback() {
        let f = Fake::new("exit 0");
        let snapshot = idle_route(&f);
        route::update(&f.0, "alice", |r| {
            r.generation += 1;
            r.suspended = true;
            Ok(())
        })
        .unwrap();
        let invoked = std::cell::Cell::new(false);
        assert_eq!(
            spawn_current(&f.0, "alice", &snapshot, || invoked.set(true)).unwrap(),
            Err(WakeStatus::Suspended)
        );
        assert!(!invoked.get());
        // Even a complete same-session update invalidates the old preparation.
        route::update(&f.0, "alice", |r| {
            r.suspended = false;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            spawn_current(&f.0, "alice", &snapshot, || invoked.set(true)).unwrap(),
            Err(WakeStatus::Waiting)
        );
        assert!(!invoked.get());
    }

    #[test]
    fn observation_lock_covers_spawn_but_is_released_before_waiting() {
        let f = Fake::new("exit 0");
        let snapshot = idle_route(&f);
        let common = f.0.clone();
        let (enter, entering) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            entering.recv().unwrap();
            route::update(&common, "alice", |r| {
                r.suspended = true;
                Ok(())
            })
            .unwrap();
            done.send(()).unwrap();
        });
        let child = spawn_current(&f.0, "alice", &snapshot, || {
            enter.send(()).unwrap();
            let lock = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(f.0.join("mdkg/hooks/receiver/alice.lock"))
                .unwrap();
            assert!(
                matches!(lock.try_lock(), Err(fs::TryLockError::WouldBlock)),
                "observation lock must cover spawn regardless of writer scheduling"
            );
            f.prepare().and_then(|c| c.spawn_before(u64::MAX))
        })
        .unwrap()
        .unwrap()
        .unwrap();
        // The status writer can now finish even though the queue child has
        // not been waited for, avoiding a child-hook/parent lock deadlock.
        finished.recv_timeout(STARTUP_ALLOWANCE).unwrap();
        writer.join().unwrap();
        assert_eq!(f.wait(child, || Ok(())), route::Delivery::Queued);
    }
    #[test]
    fn lease_loss_during_final_registry_wait_refuses_spawn() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let f = Fake::new("printf queued > invoked");
        let snapshot = idle_route(&f);
        let registry = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(f.0.join("mdkg/hooks/threads.lock"))
            .unwrap();
        registry.lock().unwrap();
        let probe = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(f.0.join("mdkg/hooks/receiver/alice.lock"))
            .unwrap();
        let owns = Arc::new(AtomicBool::new(true));
        let control = owns.clone();
        let common = f.0.clone();
        let dispatcher = std::thread::spawn(move || {
            spawn_current_held(
                &common,
                "alice",
                &snapshot,
                || {
                    if owns.load(Ordering::SeqCst) {
                        Ok(u64::MAX)
                    } else {
                        Err(invalid("lease lost"))
                    }
                },
                |_| panic!("must not spawn after lease loss"),
            )
        });
        wait_for_lock(&probe, &dispatcher);
        control.store(false, Ordering::SeqCst);
        registry.unlock().unwrap();
        assert!(dispatcher.join().unwrap().is_err());
    }

    #[test]
    fn native_guard_termination_cleans_queue_under_dash_and_bash() {
        use rustix::process::{
            Pid, Signal, WaitId, WaitIdOptions, kill_process, test_kill_process, waitid,
        };

        // Keep the direct group leader unreaped until cleanup. Even after it
        // exits, this owns its PID and prevents group-id reuse while testing.
        struct OwnedGroup(QueueChild);
        impl Drop for OwnedGroup {
            fn drop(&mut self) {
                stop_queue(&mut self.0);
            }
        }

        for shell in ["/bin/sh", "/bin/dash", "/bin/bash"] {
            if !Path::new(shell).is_file() {
                continue;
            }
            let f = Fake::new(
                "printf '%s' \"$PPID\" > native.pid; while [ ! -e release ]; do /bin/sleep 0.01; done; printf LATE > delivered",
            );
            let mut prepared = f.prepare().unwrap();
            // Run the production script and native guard with a different
            // shell; no production shell/clock selector is added. The local
            // standalone Dash lacks -p; privilege mode is not under test.
            assert_eq!(
                prepared.command.get_args().next(),
                Some(std::ffi::OsStr::new("-p"))
            );
            let mut command = Command::new(shell);
            command
                .args(prepared.command.get_args().skip(1))
                .current_dir(&f.0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(fs::File::create(f.0.join("supervisor.stderr")).unwrap())
                .process_group(0);
            for (key, value) in prepared.command.get_envs() {
                match value {
                    Some(value) => {
                        command.env(key, value);
                    }
                    None => {
                        command.env_remove(key);
                    }
                }
            }
            prepared.command = command;
            let owned = OwnedGroup(prepared.spawn_before(u64::MAX).unwrap());
            let leader = Pid::from_raw(owned.0.process.id() as i32).unwrap();
            let started = Instant::now();
            let native = loop {
                if let Some(pid) = fs::read_to_string(f.0.join("native.pid"))
                    .ok()
                    .and_then(|text| text.parse().ok())
                    .and_then(Pid::from_raw)
                {
                    break pid;
                }
                assert!(
                    started.elapsed() < STARTUP_ALLOWANCE,
                    "{shell}: native guard did not become ready"
                );
                let status = waitid(
                    WaitId::Pid(leader),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                )
                .unwrap();
                assert!(
                    status.is_none(),
                    "{shell}: group exited before readiness: {status:?}: {}",
                    fs::read_to_string(f.0.join("supervisor.stderr")).unwrap()
                );
                std::thread::sleep(super::super::guard::TICK);
            };
            let queue = Pid::from_raw(f.queue_pid()).unwrap();
            kill_process(native, Signal::TERM).expect("terminate only native guard");
            // This test never runs the receiver wait loop. Only the production
            // shell supervisor can cancel the queue after native-guard death.
            let started = Instant::now();
            let status = loop {
                if let Some(status) = waitid(
                    WaitId::Pid(leader),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                )
                .unwrap()
                {
                    break status;
                }
                assert!(
                    started.elapsed() < STARTUP_ALLOWANCE,
                    "{shell}: supervisor did not stop"
                );
                std::thread::sleep(super::super::guard::TICK);
            };
            assert!(
                status.killed(),
                "{shell}: supervisor did not kill its group: {status:?}: {}",
                fs::read_to_string(f.0.join("supervisor.stderr")).unwrap()
            );
            loop {
                match test_kill_process(queue) {
                    Err(rustix::io::Errno::SRCH) => break,
                    Ok(()) => (),
                    Err(error) => panic!("{shell}: queue lookup failed: {error}"),
                }
                assert!(
                    started.elapsed() < STARTUP_ALLOWANCE,
                    "{shell}: queue survived native guard termination"
                );
                std::thread::sleep(super::super::guard::TICK);
            }
            // Assert cancellation before fixture cleanup, so cleanup cannot
            // turn a missing production kill into a passing result.
            fs::write(f.0.join("release"), "go").unwrap();
            assert!(
                !f.0.join("delivered").exists(),
                "{shell}: queue delivered late"
            );
            drop(owned);
        }
    }

    #[test]
    fn queue_renews_ownership_and_stops_on_loss_without_retry() {
        let f = Fake::new(
            "while [ ! -e release ]; do /bin/sleep 0.01; done; printf delivered > delivered",
        );
        let mut child = f.spawn();
        f.wait_started(&mut child);
        let queue = f.queue_pid();
        let mut renewals = 0;
        let delivery = f.wait(child, || {
            renewals += 1;
            if renewals < 3 {
                Ok(())
            } else {
                Err(invalid("lease lost"))
            }
        });
        assert_eq!(delivery, route::Delivery::DeliveryUnknown);
        assert_eq!(renewals, 3);
        wait_pid_gone(queue);
        fs::write(f.0.join("release"), "go").unwrap();
        assert!(!f.0.join("delivered").exists());
    }

    #[test]
    fn blocked_renewal_cannot_extend_queue_beyond_admitted_lease() {
        let f = Fake::new(
            "while [ ! -e release ]; do /bin/sleep 0.01; done; printf delivered > delivered",
        );
        let timeout = Duration::from_secs(5);
        let mut child = f.spawn();
        f.wait_started(&mut child);
        let queue = f.queue_pid();
        let pid = child.process.id();
        let delivery = f.wait(child, || {
            f.set_clock(u64::MAX);
            // Keep the receiver inside renewal until the independent guard
            // exits. An omitted guard deadline check fails this barrier.
            wait_exited(pid);
            Ok(())
        });
        assert_eq!(delivery, route::Delivery::DeliveryUnknown);
        wait_pid_gone(queue);
        fs::write(f.0.join("release"), "go").unwrap();
        assert!(!f.0.join("delivered").exists());
        assert!(matches!(
            spawn_queue_before(
                &f.0.join("fake"),
                &f.0,
                "thread",
                "header",
                timeout,
                state::now_ms()
            ),
            Err(route::Delivery::NotSent)
        ));
    }

    #[test]
    fn pause_after_final_ownership_check_cannot_restart_deadline() {
        let f = Fake::new("printf queued > invoked");
        let snapshot = idle_route(&f);
        // Resolve both executables and prepare the command before renewal.
        let prepared = QueueCommand::prepare(
            &f.0.join("fake"),
            &f.0,
            SESSION_A,
            "headers",
            Duration::from_secs(5),
        )
        .unwrap();
        let mut checked = false;
        let result = spawn_current_held(
            &f.0,
            "alice",
            &snapshot,
            || {
                checked = true;
                Ok(state::now_ms() + 100)
            },
            |deadline| {
                // Models descheduling after ownership verification, before
                // process creation. Neither the guard nor Codex may launch.
                std::thread::sleep(Duration::from_millis(150));
                prepared.spawn_before(deadline)
            },
        )
        .unwrap();
        assert!(checked);
        assert!(matches!(result, Ok(Err(route::Delivery::NotSent))));
        assert!(!f.0.join("invoked").exists());
    }

    #[test]
    fn successful_exit_observed_after_absolute_deadline_is_unknown() {
        let f = Fake::new("exit 0");
        assert_eq!(f.run(), route::Delivery::Queued);
        let child = f.spawn();
        // Confirm success occurred in time, then suspend the receiver's
        // receipt processing. It must still refuse a post-deadline success.
        let child = wait_success(child, QUEUE_WATCHDOG);
        f.set_clock(u64::MAX);
        assert_eq!(f.wait(child, || Ok(())), route::Delivery::DeliveryUnknown);
    }

    #[test]
    fn guard_does_not_pass_child_exit_125_as_non_delivery_proof() {
        let f = Fake::new("exit 125");
        assert_eq!(f.run(), route::Delivery::DeliveryUnknown);
    }

    #[test]
    fn marker_after_registry_reconciliation_refuses_spawn() {
        let f = Fake::new("printf queued > invoked");
        let snapshot = idle_route(&f);
        let result = spawn_current_held(
            &f.0,
            "alice",
            &snapshot,
            || {
                // Renewal follows registry reconciliation, with both locks
                // held. Inject here instead of hoping a worker passed the
                // initial pending check during a fixed readiness sleep. The
                // final admission check must catch even this later marker.
                route::begin_at(
                    &f.0,
                    "alice",
                    SESSION_B,
                    &f.0,
                    &observation(hooks::HarnessEvent::SessionStart),
                    || Err(invalid("injected death after durable marker")),
                    || panic!("route work must not happen"),
                )
                .unwrap_err();
                assert!(super::super::evidence::pending(&f.0, "alice").unwrap());
                Ok(u64::MAX)
            },
            |_| panic!("must not spawn after the marker"),
        )
        .unwrap();
        assert_eq!(result, Err(WakeStatus::Suspended));
    }
}
