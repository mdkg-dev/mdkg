//! The git process groups this process started and hasn't reaped, so a
//! SIGTERM or SIGINT can reach them before mdkg exits (o-omni, Omni's
//! ask: a caller that kills mdkg must not see a git write land after it
//! reported the outcome unknown).
//!
//! Every git starts in its own process group, so a signal sent to mdkg
//! never reaches it on its own. [`stop`] forwards the signal to each group,
//! waits up to [`TERM_GRACE`], kills what's left, and reaps every leader it
//! can. From the moment it starts, no new git starts, and a thread that
//! finishes its git doesn't go on: it waits for the process to end, so no
//! later step acts on a git that the stop cut short.

use std::io;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};

/// How long git's groups get to exit after the forwarded signal, before
/// they're killed.
pub const TERM_GRACE: Duration = Duration::from_secs(2);

/// How long a stop then waits for killed groups to be gone and reaped.
pub const KILL_GRACE: Duration = Duration::from_secs(1);

/// A started git: its group id (its own pid) and, while its owner holds
/// it, the child, so a stop can reap it.
struct Group {
    pid: u32,
    child: Weak<Mutex<Child>>,
}

struct Registry {
    groups: Vec<Group>,
    stopping: bool,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    groups: Vec::new(),
    stopping: false,
});

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Parks the calling thread for good: the process is stopping, and the
/// stop ends it.
fn park() -> ! {
    loop {
        std::thread::park();
    }
}

/// Starts `cmd` in a process group of its own and tracks the group until
/// [`Tracked`] is dropped, so a stop reaches it and everything it starts.
/// Every git mdkg runs outside [`crate::Git::run`] and
/// [`crate::ObjectReader`] starts here too. Once a stop has begun,
/// nothing starts: the calling thread waits for the process to end.
pub fn spawn(cmd: &mut Command) -> io::Result<Tracked> {
    use std::os::unix::process::CommandExt as _;
    cmd.process_group(0);
    let mut registry = registry();
    if registry.stopping {
        drop(registry);
        park();
    }
    let child = cmd.spawn()?;
    let pid = child.id();
    let child = Arc::new(Mutex::new(child));
    registry.groups.push(Group {
        pid,
        child: Arc::downgrade(&child),
    });
    Ok(Tracked { pid, child })
}

/// A started git, tracked until dropped. Drop it only once the child is
/// reaped (or killed and waited for).
#[derive(Debug)]
pub struct Tracked {
    pid: u32,
    child: Arc<Mutex<Child>>,
}

impl Tracked {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// The child, for its owner's waits and kills. A stop takes it only
    /// to reap it, which `Child` remembers, so either side sees the status.
    pub fn child(&self) -> MutexGuard<'_, Child> {
        self.child.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        let mut registry = registry();
        registry.groups.retain(|g| g.pid != self.pid);
        if registry.stopping {
            drop(registry);
            park();
        }
    }
}

/// Whether a stop has begun.
pub fn stopping() -> bool {
    registry().stopping
}

/// The signal [`stop`] forwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Forward {
    Term,
    Int,
}

/// Forwards `signal` to every git group this process started, waits up to
/// [`TERM_GRACE`] for them to exit, kills the rest, and waits up to
/// [`KILL_GRACE`] more for them to be gone, reaping each leader whose
/// owner holds it. From the call on, no git starts and no thread returns
/// from one. The caller then ends the process.
pub fn stop(signal: Forward) {
    let signal = match signal {
        Forward::Term => Signal::TERM,
        Forward::Int => Signal::INT,
    };
    let groups: Vec<(u32, Weak<Mutex<Child>>)> = {
        let mut registry = registry();
        registry.stopping = true;
        registry
            .groups
            .iter()
            .map(|g| (g.pid, g.child.clone()))
            .collect()
    };
    let mut live: Vec<_> = groups
        .into_iter()
        .filter_map(|(pid, child)| Some((Pid::from_raw(i32::try_from(pid).ok()?)?, child)))
        .collect();
    for (pid, _) in &live {
        kill_process_group(*pid, signal).ok();
    }
    wait_gone(&mut live, TERM_GRACE);
    for (pid, _) in &live {
        kill_process_group(*pid, Signal::KILL).ok();
    }
    wait_gone(&mut live, KILL_GRACE);
}

/// Waits until every group in `live` is gone, or `grace` passes, keeping
/// in `live` only those still there.
fn wait_gone(live: &mut Vec<(Pid, Weak<Mutex<Child>>)>, grace: Duration) {
    let deadline = Instant::now() + grace;
    loop {
        live.retain(|(pid, child)| {
            // Reap the leader if its owner isn't holding it right now:
            // a zombie still counts as a member of its group.
            if let Some(child) = child.upgrade()
                && let Ok(mut child) = child.try_lock()
            {
                child.try_wait().ok();
            }
            test_kill_process_group(*pid).is_ok()
        });
        if live.is_empty() || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// [`Command::output`] through [`spawn`]: stdin closed, both outputs
/// collected, waited for to the end with no time limit of its own (for a
/// short command; a long one wants [`crate::Git::run`]'s bound).
pub fn output(cmd: &mut Command) -> io::Result<std::process::Output> {
    use std::io::Read as _;
    use std::process::Stdio;
    let child = spawn(
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?;
    let (out, err) = {
        let mut c = child.child();
        (c.stdout.take(), c.stderr.take())
    };
    let read = |pipe: Option<Box<dyn io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                pipe.read_to_end(&mut buf).map(|_| buf)
            } else {
                Ok(buf)
            }
        })
    };
    let stdout = read(out.map(|p| Box::new(p) as Box<dyn io::Read + Send>));
    let stderr = read(err.map(|p| Box::new(p) as Box<dyn io::Read + Send>));
    let status = loop {
        let waited = child.child().try_wait()?;
        if let Some(status) = waited {
            break status;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let joined = |h: std::thread::JoinHandle<io::Result<Vec<u8>>>| {
        h.join()
            .unwrap_or_else(|_| Err(io::Error::other("output reader panicked")))
    };
    Ok(std::process::Output {
        status,
        stdout: joined(stdout)?,
        stderr: joined(stderr)?,
    })
}
