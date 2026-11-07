//! `starting` and `spawned` (the edd's turn.starting and turn.spawned),
//! and the live-turn record's kernel form (H14).
//!
//! `starting` journals `harness.starting` and writes the record with the
//! turn's actor and worktree and no process; `spawned` adds the harness's
//! process, which is also its process group, with its start time as the
//! kernel reads it. The record is written beside today's locked launcher
//! record in `<common>/mdkg/hooks/turns/`, as `<turn id>.seam`, and removed
//! when the runner drops it at the turn's end.
//!
//! Step 1 writes it and nothing reads it but its test: the outside-turn
//! check still reads the launcher's locked record. A record that can't be
//! written only warns, so the turn runs as it did without one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Harness;
use crate::error::RunError;
use crate::turn::journal;

/// The record's schema.
pub const SCHEMA: &str = "mdkg.seam.live_turn.v1";

/// One live turn, as the kernel records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveRecord {
    pub schema: String,
    pub turn_id: String,
    /// The agent the turn runs as.
    pub actor: String,
    /// The folder the harness runs in.
    pub worktree: PathBuf,
    /// The harness's name, as the runner gives it.
    pub harness: String,
    /// The harness's process, once spawned.
    #[serde(default)]
    pub process: Option<LiveProcess>,
}

/// A harness process, by pid and start time, and the process group it
/// leads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveProcess {
    pub pid: u32,
    /// Its start time as the process table gives it; empty when the
    /// process was gone before it could be read.
    pub started: String,
    pub group: u32,
}

/// Where `turn_id`'s record is kept.
pub fn path(common: &Path, turn_id: &str) -> PathBuf {
    crate::hooks::dir(common)
        .join("turns")
        .join(format!("{turn_id}.seam"))
}

/// `turn_id`'s record, while it's kept.
pub fn read(common: &Path, turn_id: &str) -> Option<LiveRecord> {
    let bytes = std::fs::read(path(common, turn_id)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `starting`'s request, in-process.
pub(crate) struct StartingRequest<'a> {
    pub(crate) common: &'a Path,
    pub(crate) turn_id: &'a str,
    pub(crate) actor: &'a str,
    pub(crate) worktree: &'a Path,
    pub(crate) harness: Harness,
}

/// A written record, removed when dropped.
pub(crate) struct LiveTurn {
    path: PathBuf,
    record: LiveRecord,
    /// The harness's start time, being read off the turn's path.
    start_time: Option<std::sync::mpsc::Receiver<String>>,
}

impl LiveTurn {
    fn write(&self) -> std::io::Result<()> {
        let dir = self.path.parent().expect("the turns folder");
        std::fs::create_dir_all(dir)?;
        let staged = self.path.with_extension("seam.tmp");
        let bytes = serde_json::to_vec(&self.record).map_err(std::io::Error::other)?;
        let written =
            std::fs::write(&staged, bytes).and_then(|()| std::fs::rename(&staged, &self.path));
        if written.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        written
    }
}

impl Drop for LiveTurn {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// From here the harness may have run: `harness.starting` in the journal,
/// synced, then the live-turn record without a process. A journal that
/// can't be written refuses the spawn, as before; a record that can't be
/// written only warns.
pub(crate) fn starting(
    journal: Option<&mut journal::Writer>,
    req: StartingRequest<'_>,
) -> Result<Option<LiveTurn>, RunError> {
    if let Some(writer) = journal {
        writer.harness_starting(req.harness)?;
    }
    let live = LiveTurn {
        path: path(req.common, req.turn_id),
        record: LiveRecord {
            schema: SCHEMA.into(),
            turn_id: req.turn_id.into(),
            actor: req.actor.into(),
            worktree: req.worktree.to_path_buf(),
            harness: req.harness.as_str().into(),
            process: None,
        },
        start_time: None,
    };
    match live.write() {
        Ok(()) => Ok(Some(live)),
        Err(e) => {
            warn(&e);
            Ok(None)
        }
    }
}

/// The harness's process is running as `pid`, the leader of its own
/// process group: the record names it at once. Its start time, which
/// takes a process-table read, is read on a thread of its own, so nothing
/// on the turn's path waits for it (review of 3a6d8300): the runner's
/// watch loop calls [`LiveTurn::settle`] to add it once it's read.
pub(crate) fn spawned(live: Option<&mut LiveTurn>, pid: u32) {
    spawned_with(live, pid, |pid| {
        crate::turn::started(pid).ok().flatten().unwrap_or_default()
    });
}

fn spawned_with(live: Option<&mut LiveTurn>, pid: u32, read: fn(u32) -> String) {
    let Some(live) = live else {
        return;
    };
    live.record.process = Some(LiveProcess {
        pid,
        started: String::new(),
        group: pid,
    });
    if let Err(e) = live.write() {
        warn(&e);
    }
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sent.send(read(pid));
    });
    live.start_time = Some(received);
}

impl LiveTurn {
    /// Adds the harness's start time once it has been read, without
    /// waiting for it.
    pub(crate) fn settle(&mut self) {
        let Some(received) = &self.start_time else {
            return;
        };
        let started = match received.try_recv() {
            Ok(started) => started,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => String::new(),
        };
        self.start_time = None;
        if started.is_empty() {
            return;
        }
        if let Some(process) = self.record.process.as_mut() {
            process.started = started;
        }
        if let Err(e) = self.write() {
            warn(&e);
        }
    }
}

fn warn(e: &std::io::Error) {
    eprintln!(
        "warning: can't write the turn's live record: {}",
        crate::digest::terminal_safe(&e.to_string())
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_records_the_turn_spawned_adds_its_process_and_the_end_removes_it() {
        let common = std::env::temp_dir().join(format!("mdkg-seam-live-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&common).expect("common");
        let turn_id = uuid::Uuid::now_v7().to_string();
        let worktree = common.join("wt");
        // Today's locked launcher records are `.json`; this one never is,
        // so the outside-turn scan, which reads only those, skips it.
        assert_eq!(
            path(&common, &turn_id).extension().and_then(|e| e.to_str()),
            Some("seam")
        );
        let mut live = starting(
            None,
            StartingRequest {
                common: &common,
                turn_id: &turn_id,
                actor: "coder",
                worktree: &worktree,
                harness: Harness::Codex,
            },
        )
        .expect("starting")
        .expect("written");
        assert_eq!(
            read(&common, &turn_id),
            Some(LiveRecord {
                schema: SCHEMA.into(),
                turn_id: turn_id.clone(),
                actor: "coder".into(),
                worktree: worktree.clone(),
                harness: "codex".into(),
                process: None,
            })
        );
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("a process");
        spawned(Some(&mut live), child.id());
        let process = read(&common, &turn_id)
            .and_then(|r| r.process)
            .expect("the process");
        assert_eq!(process.pid, child.id());
        assert_eq!(process.group, child.id());
        // Its start time follows, once read.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let process = loop {
            live.settle();
            let process = read(&common, &turn_id)
                .and_then(|r| r.process)
                .expect("the process");
            if !process.started.is_empty() || std::time::Instant::now() > deadline {
                break process;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(
            process.started,
            crate::turn::started(child.id())
                .ok()
                .flatten()
                .unwrap_or_default()
        );
        assert!(!process.started.is_empty(), "a live process's start time");
        let _ = child.kill();
        let _ = child.wait();
        drop(live);
        assert!(read(&common, &turn_id).is_none());
        assert!(!path(&common, &turn_id).exists());
        let _ = std::fs::remove_dir_all(&common);
    }

    #[test]
    fn a_slow_start_time_read_never_holds_up_the_turn() {
        let common = std::env::temp_dir().join(format!("mdkg-seam-live-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&common).expect("common");
        let mut live = starting(
            None,
            StartingRequest {
                common: &common,
                turn_id: "t-slow",
                actor: "coder",
                worktree: &common,
                harness: Harness::Claude,
            },
        )
        .expect("starting")
        .expect("written");
        fn slow(_: u32) -> String {
            std::thread::sleep(std::time::Duration::from_secs(2));
            "Wed Nov  4 00:00:00 2026".into()
        }
        let begun = std::time::Instant::now();
        spawned_with(Some(&mut live), 4242, slow);
        live.settle();
        assert!(
            begun.elapsed() < std::time::Duration::from_secs(1),
            "spawned and settle return before the read ends"
        );
        let process = read(&common, "t-slow")
            .and_then(|r| r.process)
            .expect("the pid");
        assert_eq!(
            (process.pid, process.group, process.started.as_str()),
            (4242, 4242, "")
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while live.start_time.is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            live.settle();
        }
        let process = read(&common, "t-slow")
            .and_then(|r| r.process)
            .expect("the pid");
        assert_eq!(process.started, "Wed Nov  4 00:00:00 2026");
        drop(live);
        let _ = std::fs::remove_dir_all(&common);
    }

    #[test]
    fn a_record_that_cant_be_written_only_warns() {
        let common = std::env::temp_dir().join(format!("mdkg-seam-live-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(crate::hooks::dir(&common)).expect("hooks");
        // A file where the turns folder goes.
        std::fs::write(crate::hooks::dir(&common).join("turns"), "").expect("blocker");
        let live = starting(
            None,
            StartingRequest {
                common: &common,
                turn_id: "t-1",
                actor: "coder",
                worktree: &common,
                harness: Harness::OpenCode,
            },
        )
        .expect("the turn goes on");
        assert!(live.is_none());
        let _ = std::fs::remove_dir_all(&common);
    }
}
