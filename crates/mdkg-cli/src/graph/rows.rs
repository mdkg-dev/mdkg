//! Graph sync's doctor rows and the launcher's timers (edd-01a18867,
//! Config "Doctor checks" and Sync "When it runs"), on [`super::sync`].

use std::path::Path;

use mdkg_graph::graphref::{self, GraphRef};
use mdkg_graph::sync::{self, Approval, Who};

use super::sync::{protected, run_once, time_text, tip};
use crate::output;

/// How a doctor row reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warning,
    Error,
}

/// One doctor row: its level, message and fix.
#[derive(Debug, Clone)]
pub struct Row {
    pub level: Level,
    pub message: String,
    pub hint: Option<String>,
}

fn row(level: Level, message: impl Into<String>, hint: Option<String>) -> Row {
    Row {
        level,
        message: message.into(),
        hint,
    }
}

/// Doctor's `graph.remote` row past the checks doctor makes itself (the
/// remote exists, remoteRef isn't protected): the destination guard, the
/// approval (G23), and the last sync's state (edd-01a18867, Config,
/// "Doctor checks"). Never approved is a warning, not an error: personal
/// install sets `graph.remote` and nothing is pushed until a person
/// approves, which `doctor --strict` names.
pub fn remote_row(top: &Path, graph: &GraphRef) -> Option<Row> {
    let remote = graph.remote.as_ref()?;
    let bin = mdkg_format::program::BIN;
    // Doctor runs where sync would: the invoking checkout, `top`.
    let (now, base) = match sync::current(top, graph)
        .and_then(|d| sync::net::checkout(&graph.common, top).map(|b| (d, b)))
    {
        Ok(v) => v,
        Err(e) => return Some(row(Level::Error, e.message, None)),
    };
    if let Err(e) = sync::guard::check_remote_ref(
        &graph.common,
        &now.remote,
        &now.remote_ref,
        graph.main_ref.as_deref(),
        &protected(top, graph),
    ) {
        return Some(row(Level::Error, e.message, None));
    }
    if let Err(e) = sync::guard::check_destination(
        &graph.common,
        &base,
        &now.remote,
        &now,
        graph.main_ref.as_deref(),
    ) {
        return Some(row(
            Level::Error,
            e.message,
            Some("point graph.remote at a repository of its own (`git config mdkg.graph.remote <name>`)".into()),
        ));
    }
    match sync::approval(top, graph) {
        Ok(Approval::Approved) => {}
        Ok(Approval::Unapproved) => {
            return Some(row(
                Level::Warning,
                format!(
                    "{} ({}), not approved: nothing is pulled from or pushed to it",
                    remote.name, remote.remote_ref
                ),
                Some(format!(
                    "a person checks the destination and runs `{bin} graph remote approve`"
                )),
            ));
        }
        Ok(Approval::Changed(why)) => {
            return Some(row(
                Level::Error,
                format!(
                    "{}: the destination changed since it was approved ({why})",
                    remote.name
                ),
                Some(format!(
                    "check the remote's URLs, then `{bin} graph remote approve` again"
                )),
            ));
        }
        Err(e) => return Some(row(Level::Error, e.message, None)),
    }
    let state = sync::State::load(&graph.common).unwrap_or_default();
    if let Some(r) = &state.refused {
        // The latch of a push whose outcome is unknown says so; only a
        // recorded refusal names a hook (Terra #17986).
        let (message, hint) = if r.code == sync::UNRECORDED {
            (
                format!("{} ({})", r.shown(), time_text(r.time)),
                format!("check the remote's graph ref, then `{bin} graph sync`"),
            )
        } else {
            (
                format!(
                    "a hook refused the last graph push at {}: {}",
                    time_text(r.time),
                    r.shown()
                ),
                format!("fix what the hook refuses, then `{bin} graph sync`"),
            )
        };
        // A deleted remote graph ref is named here too (Terra #19803).
        let message = match (state.missing, state.recreate) {
            (true, true) => format!(
                "{message}; {} on {} is deleted, and the next push recreates it",
                remote.remote_ref, remote.name
            ),
            (true, false) => format!(
                "{message}; {} on {} is deleted, and recreating it needs `{bin} graph remote approve`",
                remote.remote_ref, remote.name
            ),
            _ => message,
        };
        return Some(row(Level::Warning, message, Some(hint)));
    }
    // A deleted remote graph ref (DECISION 17), whatever the last sync
    // did: a pull-only sync succeeds against it (Terra #19640). A later
    // failure of another kind (a push the scan refused) comes first, with
    // the deletion named (Terra #19723).
    if state.missing
        && let Some(f) = &state.last_error
        && f.code != "graph.remote_rewritten"
    {
        return Some(row(
            Level::Warning,
            format!(
                "the last sync failed at {} ({}): {}; {} on {} is deleted",
                time_text(f.time),
                f.code,
                f.shown(),
                remote.remote_ref,
                remote.name
            ),
            Some(format!("fix that, then `{bin} graph sync`")),
        ));
    }
    if state.missing && state.recreate {
        return Some(row(
            Level::Warning,
            format!(
                "{} on {} is deleted; approved again, so the next push creates it",
                remote.remote_ref, remote.name
            ),
            Some(format!("`{bin} graph sync` pushes now")),
        ));
    }
    if state.missing {
        return Some(row(
            Level::Warning,
            format!(
                "{} on {} was deleted after this clone accepted a tip: nothing is pushed there",
                remote.remote_ref, remote.name
            ),
            Some(format!(
                "check the remote's graph ref, then `{bin} graph remote approve`"
            )),
        ));
    }
    if let Some(f) = &state.last_error {
        let hint = if f.code == "graph.remote_rewritten" {
            format!("check the remote's graph ref, then `{bin} graph sync --accept-rewrite`")
        } else {
            format!("`{bin} graph sync` retries now")
        };
        return Some(row(
            Level::Warning,
            format!(
                "the last sync failed at {} ({}): {}",
                time_text(f.time),
                f.code,
                f.shown()
            ),
            Some(hint),
        ));
    }
    // A listing that fails (a conflict's merge lost behind a shallow
    // boundary, say) is shown, never read as none (Terra #19873).
    let open = match sync::conflicts(&graph.common, tip(graph).as_deref()) {
        Ok(open) => open,
        Err(e) => {
            return Some(row(
                Level::Warning,
                format!(
                    "the sync conflicts can't be listed ({}): {}",
                    e.code, e.message
                ),
                Some(format!("fix that, then `{bin} graph conflicts`")),
            ));
        }
    };
    if !open.is_empty() {
        return Some(row(
            Level::Warning,
            format!("{} unresolved sync conflict(s)", open.len()),
            Some(format!(
                "`{bin} graph conflicts`, then `{bin} graph resolve <path> --take local|remote`"
            )),
        ));
    }
    if state.accepted.is_none() && tip(graph).is_some() {
        // Approved, and nothing pushed or pulled yet (Terra #18237 minor).
        return Some(row(
            Level::Warning,
            format!(
                "{} ({}), approved; not synced yet, so the local graph isn't on the remote",
                remote.name, remote.remote_ref
            ),
            Some(format!(
                "`{bin} graph sync`, or let `agents up` sync on its timers"
            )),
        ));
    }
    if let (Some(t), Some(a)) = (tip(graph), &state.accepted) {
        let count = |range: &str| {
            mdkg_git::common_command(&graph.common)
                .args(["rev-list", "--count", range])
                .run(None)
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .parse::<u64>()
                        .ok()
                })
                .unwrap_or(0)
        };
        let (ahead, behind) = (count(&format!("{a}..{t}")), count(&format!("{t}..{a}")));
        if ahead > 0 || behind > 0 {
            return Some(row(
                Level::Warning,
                format!(
                    "{} ({}), approved; {ahead} local graph commit(s) not pushed, {behind} not pulled",
                    remote.name, remote.remote_ref
                ),
                Some(format!(
                    "`{bin} graph sync`, or let `agents up` sync on its timers"
                )),
            ));
        }
    }
    Some(row(
        Level::Ok,
        format!("{} ({}), approved", remote.name, remote.remote_ref),
        None,
    ))
}

/// Doctor's `graph.secret_scan` row: pushes need the scanner, which runs
/// `git`.
pub fn secret_scan_row(graph: &GraphRef) -> Option<Row> {
    graph.remote.as_ref()?;
    let ok = std::process::Command::new("git")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success());
    Some(if ok {
        row(
            Level::Ok,
            "built in (secret.scan); graph pushes are scanned",
            None,
        )
    } else {
        row(
            Level::Warning,
            "the secret scanner can't run git, so graph pushes are refused",
            Some("put git on PATH".into()),
        )
    })
}

/// The launcher's graph sync (G13, answer (b): in `agents up`): a pull at
/// start and every `pullInterval`, a push `pushDelay` after the last local
/// graph change, coalesced. Background syncs go only to an approved,
/// unchanged destination (the engine refuses anything else), never run
/// inside a turn, and never retry around a hook. Each distinct outcome or
/// failure is printed once.
pub struct Syncer {
    timer: sync::Timer,
    last: Option<String>,
}

impl Syncer {
    /// The first pull, run before `agents up` admits any turn (Terra
    /// #18237), so turns start on what the remote holds. The caller has
    /// checked that this runs outside any agent turn (DECISION 19 (T1)).
    pub fn start(top: &Path, graph: &GraphRef, json: bool) -> Self {
        let now = std::time::Instant::now();
        let mut s = Self {
            timer: sync::Timer::new(graph.push_delay, graph.pull_interval, now, tip(graph)),
            last: None,
        };
        // With no remote yet, the first pull waits for one.
        if graph.remote.is_some() {
            s.tick(top, graph, now, json);
        }
        s.notices();
        s
    }

    /// Ticks once a second until `drained` (every turn finished, Terra
    /// #18237), reading the graph settings again on each tick, so a changed
    /// `pushDelay`, `pullInterval` or remote applies without a restart.
    /// When `up` stops, a push still waiting for its delay is dropped
    /// (2026-11-04): the next `agents up` or a person's `graph sync`
    /// sends it.
    pub fn run(mut self, top: &Path, drained: &dyn Fn() -> bool, json: bool) {
        loop {
            if drained() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            self.notices();
            if drained() {
                break;
            }
            let graph = match graphref::resolve(top) {
                Ok(Some(g)) if g.remote.is_some() => g,
                // Local only (so far): nothing to sync, and nothing to say.
                Ok(_) => continue,
                Err(e) => {
                    self.say(format!("graph sync: {}: {}", e.code, e.message), json);
                    continue;
                }
            };
            let now = std::time::Instant::now();
            self.timer
                .set_cadence(graph.push_delay, graph.pull_interval, now);
            self.tick(top, &graph, now, json);
            // What the sync noticed, before `drained` can end the loop
            // (Terra #19704).
            self.notices();
        }
        self.notices();
    }

    /// One sync, when the timer says one is due for the destination
    /// approved now.
    fn tick(&mut self, top: &Path, graph: &GraphRef, now: std::time::Instant, json: bool) {
        let current = tip(graph);
        let approved = graph.remote.as_ref().and_then(|r| r.pinned.as_ref());
        if let Some(due) = self.timer.due(now, current.as_deref(), approved) {
            self.sync(top, graph, due, approved, json);
        }
    }

    fn say(&mut self, text: String, json: bool) {
        if self.last.as_deref() != Some(&text) && !json {
            eprintln!("agents up: {}", mdkg_run::digest::terminal_safe(&text));
        }
        self.last = Some(text);
    }

    /// One sync: a due pull only pulls; only a due push pushes.
    fn sync(
        &mut self,
        top: &Path,
        graph: &GraphRef,
        due: sync::Due,
        approved: Option<&graphref::Destination>,
        json: bool,
    ) {
        let push = due == sync::Due::Push;
        match run_once(top, graph, Who::Launcher, push, false) {
            Ok(out) => {
                // Recorded for the destination approved when it started; a
                // pin changed since is pulled from again.
                self.timer.succeeded(std::time::Instant::now(), approved);
                if out.after != out.before {
                    self.timer.moved(Some(&out.after));
                }
                if out.ahead {
                    self.timer.ahead(std::time::Instant::now());
                }
                let what = match (out.pushed, out.after != out.before) {
                    (true, _) => format!("graph pushed to {}", out.remote),
                    (false, true) => format!("graph pulled from {}", out.remote),
                    _ => String::new(),
                };
                if what.is_empty() {
                    self.last = None;
                } else {
                    self.say(what, json);
                }
                for c in &out.conflicts {
                    self.say(
                        format!(
                            "graph sync took the remote's version of {}; `{} graph conflicts` lists it",
                            output::shown_path(&c.path),
                            mdkg_format::program::BIN
                        ),
                        json,
                    );
                }
            }
            Err(e) => {
                // A failure or refusal records nothing; it only backs off
                // (DECISION 19 (T2)).
                self.timer.failed(due, std::time::Instant::now());
                self.say(format!("graph sync: {}: {}", e.code, e.message), json)
            }
        }
    }

    /// What was noticed since the last tick (a replay of pending writes,
    /// Terra #18984), with its hint: `up` prints no command envelope, so
    /// the syncer prints them, on every tick (Terra #19208 m4).
    fn notices(&self) {
        for d in output::take_notices() {
            let hint = d
                .hint
                .as_deref()
                .map(|h| format!(" ({})", mdkg_run::digest::terminal_safe(h)))
                .unwrap_or_default();
            eprintln!(
                "agents up: {}: {}{hint}",
                d.code.as_str(),
                mdkg_run::digest::terminal_safe(&d.message)
            );
        }
    }
}
