//! The executor behind MCP and HTTP: an operation's JSON arguments become
//! the CLI's argv, and the call runs this binary once as a subprocess with
//! `--json`, so its result is exactly the envelope the CLI prints.
//!
//! Each adapter keeps its own policy (which arguments a tool refuses, the
//! mailbox it pins) and shares the mapping and the run.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mdkg_format::Code;
use serde_json::{Map, Value};

pub(crate) mod served;

/// The most a CLI call may print on stdout before it's stopped.
const MAX_CHILD_OUTPUT: u64 = 32 << 20;
/// The most a CLI call may print on stderr before it's stopped, envelope
/// or not: the server only quotes stderr when there's no envelope, and
/// reading on past the cap would leave the call running to its timeout.
const MAX_CHILD_STDERR: u64 = 1 << 20;

/// One argument an operation accepts.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Arg {
    /// A positional value, passed after `--` so it can't be read as a flag.
    Positional(&'static str),
    /// One or more positional values, as a string or an array of strings.
    Positionals(&'static str),
    /// `--flag=value` once per value, as a string or an array of strings.
    Repeated(&'static str, &'static str),
    /// Any JSON value, sent on the CLI's stdin for `--flag=-`.
    Data(&'static str, &'static str),
    /// `--flag=value`.
    Text(&'static str, &'static str),
    /// A comma-separated list, given as a string or an array of strings.
    List(&'static str, &'static str),
    Integer(&'static str, &'static str),
    /// A whole number of at least the third field (a cursor or seconds: 0;
    /// a limit: 1).
    Count(&'static str, &'static str, u64),
    /// `--flag` when true.
    Flag(&'static str, &'static str),
}

impl Arg {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Arg::Positional(k)
            | Arg::Positionals(k)
            | Arg::Repeated(k, _)
            | Arg::Data(k, _)
            | Arg::Text(k, _)
            | Arg::List(k, _)
            | Arg::Integer(k, _)
            | Arg::Count(k, ..)
            | Arg::Flag(k, _) => k,
        }
    }
}

/// A call's CLI arguments, and the bytes to send on its stdin (a data
/// part, read by the CLI as `--data-file=-`).
#[derive(Debug, Default)]
pub(crate) struct Prepared {
    pub(crate) args: Vec<String>,
    pub(crate) stdin: Option<Vec<u8>>,
}

/// The arguments given, mapped but not yet assembled: each flag with the
/// key it came from, so an adapter can hold one back.
#[derive(Debug, Default)]
pub(crate) struct Mapped {
    pub(crate) flags: Vec<(&'static str, String)>,
    pub(crate) positionals: Vec<String>,
    pub(crate) stdin: Option<Vec<u8>>,
}

/// A JSON number that is a whole number, such as 3 or 3.0 (some clients
/// send every number as a double), exactly: integers are read as integers,
/// and only a real double goes through f64.
pub(crate) fn whole(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
        .or_else(|| {
            // `fract()` of a non-finite number is NaN, so this refuses it.
            value
                .as_f64()
                .filter(|n| n.fract() == 0.0)
                .map(|n| n as i128)
        })
}

/// A string, or an array of strings, as a list.
pub(crate) fn strings(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(items) => items.iter().map(|i| i.as_str().map(String::from)).collect(),
        _ => None,
    }
}

/// Refuses a key `args` doesn't take, and a missing `required` one. `name`
/// is the operation's, for the message, which names the argument, never
/// its value.
pub(crate) fn check_keys(
    name: &str,
    args: &[Arg],
    required: &[&str],
    given: &Map<String, Value>,
) -> Result<(), String> {
    // An unknown key isn't echoed: a model can put anything there.
    if given
        .keys()
        .any(|k| !args.iter().any(|a| a.key() == k.as_str()))
    {
        let known: Vec<&str> = args.iter().map(|a| a.key()).collect();
        return Err(format!(
            "{name} got an argument it doesn't take; it takes {}",
            known.join(", ")
        ));
    }
    for key in required {
        if !given.contains_key(*key) {
            return Err(format!("{name} needs `{key}`"));
        }
    }
    Ok(())
}

/// Maps each given argument to its flag or positional, in `args` order. A
/// message names the argument, never its value, which may contain secrets.
pub(crate) fn map_args(args: &[Arg], given: &Map<String, Value>) -> Result<Mapped, String> {
    let mut mapped = Mapped::default();
    for arg in args {
        let Some(value) = given.get(arg.key()) else {
            continue;
        };
        let wrong = || format!("`{}` has the wrong type", arg.key());
        match *arg {
            Arg::Positional(_) => {
                mapped
                    .positionals
                    .push(value.as_str().ok_or_else(wrong)?.to_string());
            }
            Arg::Positionals(_) => mapped.positionals.extend(strings(value).ok_or_else(wrong)?),
            Arg::Repeated(key, flag) => {
                for v in strings(value).ok_or_else(wrong)? {
                    mapped.flags.push((key, format!("{flag}={v}")));
                }
            }
            // On stdin, so no temporary file holds the message.
            Arg::Data(key, flag) => {
                mapped.flags.push((key, format!("{flag}=-")));
                mapped.stdin = Some(value.to_string().into_bytes());
            }
            Arg::Text(key, flag) => {
                mapped
                    .flags
                    .push((key, format!("{flag}={}", value.as_str().ok_or_else(wrong)?)));
            }
            Arg::List(key, flag) => {
                let values = strings(value).ok_or_else(wrong)?;
                // An empty `tags` filters nothing, as before the CLI
                // refused an empty value (models fill optional arrays
                // with `[]`). Other lists go as given, for the CLI to
                // judge.
                if !(values.is_empty() && key == "tags") {
                    mapped
                        .flags
                        .push((key, format!("{flag}={}", values.join(","))));
                }
            }
            Arg::Integer(key, flag) => {
                // A double of 2^63 or more is out of range even when it
                // rounded onto i64::MIN (serde_json reads an integer below
                // i64::MIN as a double).
                let n = whole(value)
                    .filter(|n| !value.is_f64() || n.unsigned_abs() < 1 << 63)
                    .and_then(|n| i64::try_from(n).ok());
                mapped
                    .flags
                    .push((key, format!("{flag}={}", n.ok_or_else(wrong)?)));
            }
            Arg::Count(key, flag, min) => {
                let n = whole(value).ok_or_else(wrong)?;
                if n < i128::from(min) {
                    return Err(format!("`{key}` must be at least {min}"));
                }
                // Anything this large means "no limit"; every CLI flag,
                // search's i64 `--limit` too, takes i64::MAX.
                let n = n.min(i128::from(i64::MAX));
                mapped.flags.push((key, format!("{flag}={n}")));
            }
            Arg::Flag(key, flag) => {
                if value.as_bool().ok_or_else(wrong)? {
                    mapped.flags.push((key, flag.to_string()));
                }
            }
        }
    }
    Ok(mapped)
}

/// The argv for `command` (words separated by spaces): the words, `--json`,
/// the flags, then the positionals after `--`.
pub(crate) fn assemble(command: &str, flags: Vec<String>, positionals: Vec<String>) -> Vec<String> {
    let mut args: Vec<String> = command.split(' ').map(String::from).collect();
    args.push("--json".to_string());
    args.extend(flags);
    if !positionals.is_empty() {
        args.push("--".to_string());
        args.extend(positionals);
    }
    args
}

/// How long a CLI call may run, and how often it reports progress.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) timeout: Duration,
    pub(crate) progress_every: Duration,
}

/// Why a call produced no envelope.
#[derive(Debug)]
pub(crate) struct Stopped {
    pub(crate) code: Code,
    pub(crate) message: String,
    /// The CLI ran, so a write may have landed.
    pub(crate) started: bool,
}

/// Runs this binary with `args` and returns its JSON envelope, or why it
/// produced none. Stops the call when `cancelled` is set or it outlasts
/// `timeout`.
pub(crate) fn run_cli(
    exe: &Path,
    global: &[OsString],
    args: &[String],
    input: Option<&[u8]>,
    limits: Limits,
    cancelled: &AtomicBool,
    progress: &dyn Fn(Duration),
) -> Result<Value, Stopped> {
    let call = Call {
        exe,
        global,
        args,
        input,
        env_remove: &[],
    };
    run(&call, limits, cancelled, progress).map(|ran| ran.envelope)
}

/// The binary a call runs: this process's own executable, never one
/// found on PATH or through a symlink, so verifying the server's binary at
/// launch covers every child it runs. On Linux that's `/proc/self/exe`;
/// elsewhere the canonical path of `current_exe()`.
pub(crate) fn self_exe() -> std::io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let exe = PathBuf::from("/proc/self/exe");
        if exe.exists() {
            return Ok(exe);
        }
    }
    std::fs::canonicalize(std::env::current_exe()?)
}

/// One CLI call: the binary, its arguments, its stdin, and the variables
/// it must not inherit.
pub(crate) struct Call<'a> {
    pub(crate) exe: &'a Path,
    pub(crate) global: &'a [OsString],
    pub(crate) args: &'a [String],
    pub(crate) input: Option<&'a [u8]>,
    pub(crate) env_remove: &'a [&'a str],
}

/// What a call that ran printed, and how it exited.
#[derive(Debug)]
pub(crate) struct Ran {
    pub(crate) envelope: Value,
    /// The exit code; none when a signal ended it. Read by `served`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) code: Option<i32>,
}

/// [`run_cli`], with the exit code and the call's environment.
pub(crate) fn run(
    call: &Call<'_>,
    limits: Limits,
    cancelled: &AtomicBool,
    progress: &dyn Fn(Duration),
) -> Result<Ran, Stopped> {
    let Call {
        exe,
        global,
        args,
        input,
        env_remove,
    } = *call;
    let failed = |message: String| Stopped {
        code: Code::McpCliFailed,
        message,
        started: true,
    };
    let mut command = Command::new(exe);
    command
        .args(global)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in env_remove {
        command.env_remove(name);
    }
    // Its own process group, so a stopped call stops everything it started.
    // The trade-off: a signal to the server's own group (Ctrl-C) no longer
    // reaches calls in flight; they end at their own timeout. The server
    // stops them when its client goes away (MCP's stdin closes, serve's
    // socket ends) and serve's drain stops them before it exits, but a
    // server that is killed leaves them to outlive it, until their timeout
    // (the launcher's turn mark stops them after a turn).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        // The child names itself as this server was invoked, not as
        // `exe` (Linux's /proc/self/exe) or a link's target, so its
        // hints name a command that runs (Terra #15584).
        command.arg0(mdkg_format::program::name());
    }
    let mut child = command.spawn().map_err(|e| Stopped {
        started: false,
        ..failed(format!("couldn't start mdkg: {e}"))
    })?;
    #[cfg(unix)]
    register_group(child.id());
    // Feed stdin from a thread, so a child that exits without reading it
    // can't block the call.
    // Out of threads: stop the child and say so, rather than panic.
    let no_thread = |child: &mut std::process::Child| {
        kill_group(child);
        failed("the server couldn't start a thread for the call".to_string())
    };
    if let (Some(bytes), Some(mut pipe)) = (input, child.stdin.take()) {
        let bytes = bytes.to_vec();
        if std::thread::Builder::new()
            .spawn(move || pipe.write_all(&bytes).ok())
            .is_err()
        {
            return Err(no_thread(&mut child));
        }
    }
    let timeout = limits.timeout;
    // Drain both pipes while waiting, so a large envelope can't fill a pipe
    // and stall the child.
    // Each pipe keeps at most its cap. A pipe that goes over stops being
    // read and raises `overflow`, and the call stops its child.
    let overflow = Arc::new(AtomicBool::new(false));
    let drain = |pipe: Option<Box<dyn Read + Send>>, cap: u64| {
        let overflow = Arc::clone(&overflow);
        std::thread::Builder::new().spawn(move || {
            let mut bytes = Vec::new();
            if let Some(p) = pipe {
                p.take(cap + 1).read_to_end(&mut bytes).ok();
                if bytes.len() as u64 > cap {
                    overflow.store(true, Ordering::SeqCst);
                }
            }
            bytes
        })
    };
    let Ok(stdout) = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        MAX_CHILD_OUTPUT,
    ) else {
        return Err(no_thread(&mut child));
    };
    let Ok(stderr) = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        MAX_CHILD_STDERR,
    ) else {
        return Err(no_thread(&mut child));
    };
    let too_much = || failed("mdkg printed more than the server reads".to_string());
    let started = Instant::now();
    let mut reported = started;
    let exited = loop {
        if overflow.load(Ordering::SeqCst) {
            break Err(too_much());
        }
        match try_reap(&mut child) {
            Ok(Some(status)) => break Ok(status.code()),
            Ok(None) => {}
            Err(e) => break Err(failed(format!("lost the mdkg process: {e}"))),
        }
        if cancelled.load(Ordering::SeqCst) {
            // No reply goes out for a cancelled call.
            break Err(failed("cancelled".to_string()));
        }
        if started.elapsed() > timeout {
            break Err(Stopped {
                code: Code::McpCallTimeout,
                message: format!("timed out after {}s", timeout.as_secs()),
                started: true,
            });
        }
        if reported.elapsed() >= limits.progress_every {
            reported = Instant::now();
            progress(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let code = match exited {
        Ok(code) => code,
        Err(why) => {
            kill_group(&mut child);
            return Err(why);
        }
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    if overflow.load(Ordering::SeqCst) {
        return Err(too_much());
    }
    let envelope = serde_json::from_slice::<Value>(&out)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| {
            failed(format!(
                "mdkg printed no JSON envelope: {}",
                String::from_utf8_lossy(&err).trim()
            ))
        })?;
    Ok(Ran { envelope, code })
}

/// The shell fallback's script for killing process group `$1` (a negative
/// pid). `-s KILL`: dash's builtin refuses `-KILL -- -<pid>`.
pub(crate) const KILL_GROUP_SCRIPT: &str = "kill -s KILL -- \"$1\"";

/// The process groups of children started in their own group and not yet
/// reaped, each with the scope it was started in, so serve's drain can
/// kill whatever its requests left however late the thread that owns one
/// is (orchestrator #15424). Scope 0, a thread outside any serve, is
/// recorded too but never drained: an entry is what tells `kill_group` the
/// child is unreaped, so its group may still be signalled.
static LIVE: std::sync::Mutex<std::collections::BTreeMap<u32, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

std::thread_local! {
    static SCOPE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A fresh scope, one per serve.
pub(crate) fn new_scope() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

/// Children this thread starts from now on are recorded under `scope`.
pub(crate) fn enter_scope(scope: u64) {
    SCOPE.with(|s| s.set(scope));
}

/// Records a child that leads its own process group, under this thread's
/// scope. Every such child is reaped through `try_reap`, `wait_reap` or
/// `kill_group`, which forget it.
pub(crate) fn register_group(pid: u32) {
    let scope = SCOPE.with(std::cell::Cell::get);
    LIVE.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pid, scope);
}

/// Kills every group `scope` recorded that is still live, and forgets
/// them: the drain's last step, so no child outlives serve. Their owners
/// still reap them.
pub(crate) fn kill_live_groups(scope: u64) -> usize {
    // Signalled with the registry held: an owner reaps only under it
    // (`try_reap`), so no pid here can have been reaped and reused.
    let mut map = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let live: Vec<u32> = map
        .iter()
        .filter(|(_, s)| **s == scope)
        .map(|(p, _)| *p)
        .collect();
    for pid in &live {
        map.remove(pid);
        signal_group(*pid);
    }
    live.len()
}

/// `try_wait`, with the registry held, forgetting the group once it is
/// reaped: a recorded pid is never one the OS may have handed on.
pub(crate) fn try_reap(
    child: &mut std::process::Child,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let mut map = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let status = child.try_wait();
    if !matches!(status, Ok(None)) {
        map.remove(&child.id());
    }
    status
}

/// Waits for `child` to exit and reaps it through `try_reap`, so the drain
/// can still kill it while it runs.
pub(crate) fn wait_reap(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    loop {
        match try_reap(child) {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
}

/// Kills a stopped call's child and, on unix, every process it started (its
/// process group), then reaps it. Elsewhere only the child is killed.
pub(crate) fn kill_group(child: &mut std::process::Child) {
    {
        // Only a recorded child's group is signalled: recorded means
        // unreaped, so the pid is still the child's own (a zombie holds
        // it), and the group's other processes die even when the leader
        // has already exited. A child already reaped isn't signalled: its
        // pid may be another's. `kill` does nothing to a reaped child.
        let mut map = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        if map.remove(&child.id()).is_some() {
            signal_group(child.id());
        }
        child.kill().ok();
    }
    child.wait().ok();
}

/// SIGKILL to process group `pid`, on unix.
fn signal_group(pid: u32) {
    // `kill` from the usual places, else the shell's own (a builtin, so it
    // works where no kill program is installed).
    #[cfg(unix)]
    {
        let group = format!("-{pid}");
        let program = ["/bin/kill", "/usr/bin/kill"]
            .into_iter()
            .find(|k| Path::new(k).exists());
        let mut kill = match program {
            Some(k) => {
                let mut c = Command::new(k);
                c.args(["-KILL", "--", &group]);
                c
            }
            None => {
                let mut c = Command::new("/bin/sh");
                c.args(["-c", KILL_GROUP_SCRIPT, "sh", &group]);
                c
            }
        };
        kill.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[cfg(unix)]
    #[test]
    fn a_recorded_group_still_live_is_killed_and_a_reaped_one_forgotten() {
        use std::os::unix::process::CommandExt;
        let spawn = || {
            let mut c = std::process::Command::new("sleep");
            c.arg("60").process_group(0);
            let child = c.spawn().expect("sleep");
            super::register_group(child.id());
            child
        };
        let scope = super::new_scope();
        super::enter_scope(scope);
        let mut live = spawn();
        let mut done = spawn();
        super::kill_group(&mut done);
        super::enter_scope(0);
        assert_eq!(super::kill_live_groups(scope), 1);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = live.try_wait().expect("wait") {
                assert!(!status.success(), "killed, not exited");
                break;
            }
            assert!(std::time::Instant::now() < until, "the group lived on");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!super::LIVE.lock().expect("live").contains_key(&done.id()));
    }

    /// A child reaped on its own leaves the registry in the same step, so
    /// the drain has no stale pid to signal, and killing it afterwards
    /// signals nothing.
    #[cfg(unix)]
    #[test]
    fn a_child_reaped_on_its_own_leaves_nothing_for_the_drain() {
        use std::os::unix::process::CommandExt;
        let scope = super::new_scope();
        super::enter_scope(scope);
        let mut c = std::process::Command::new("true");
        c.process_group(0);
        let mut child = c.spawn().expect("true");
        super::register_group(child.id());
        super::enter_scope(0);
        let status = super::wait_reap(&mut child).expect("reaped");
        assert!(status.success());
        assert!(!super::LIVE.lock().expect("live").contains_key(&child.id()));
        super::kill_group(&mut child);
        assert_eq!(super::kill_live_groups(scope), 0);
    }

    /// A leader that exited on its own, not yet reaped, still has its
    /// group killed: what it started dies with it (Terra #15863). Pipes
    /// tell both ends, with no timing and no reaping: the leader alone
    /// holds stdout, and `ps` then shows it a zombie; the background
    /// `sleep` alone holds stderr (the leader closes its copy), so that
    /// EOF means the sleep is gone, reaped or not.
    #[cfg(unix)]
    #[test]
    fn killing_an_exited_unreaped_leader_kills_the_rest_of_its_group() {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        use std::time::Duration;
        let mut c = std::process::Command::new("/bin/sh");
        c.args([
            "-c",
            "sleep 60 </dev/null >/dev/null & exec 2>&-; echo started",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0);
        let mut child = c.spawn().expect("sh");
        super::register_group(child.id());
        let mut stderr = child.stderr.take().expect("stderr");
        let (gone, sleep_gone) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = stderr.read_to_end(&mut sink);
            let _ = gone.send(());
        });
        let mut out = String::new();
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_string(&mut out)
            .expect("leader's output");
        assert_eq!(out.trim(), "started", "the leader ran");
        // Stdout's EOF can come just before the exit is waitable: wait
        // for the leader to be a zombie, which `ps` reports without
        // reaping it (Terra #15999).
        let pid = child.id().to_string();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stat = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid])
                .output()
                .expect("ps");
            if String::from_utf8_lossy(&stat.stdout)
                .trim_start()
                .starts_with('Z')
            {
                break;
            }
            assert!(std::time::Instant::now() < until, "the leader never exited");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            sleep_gone.recv_timeout(Duration::from_millis(300)).is_err(),
            "the leader's sleep runs on"
        );
        super::kill_group(&mut child);
        assert!(
            sleep_gone.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the group outlived its leader"
        );
    }
    use mdkg_ops::registry::{NOT_OPS, OPS, not_an_op};

    /// Every CLI command by its words, and whether it takes `--json`.
    fn commands() -> Vec<(Vec<String>, bool)> {
        fn walk(c: &clap::Command, words: &mut Vec<String>, out: &mut Vec<(Vec<String>, bool)>) {
            out.push((
                words.clone(),
                c.get_arguments().any(|a| a.get_id() == "json"),
            ));
            for sub in c.get_subcommands() {
                words.push(sub.get_name().to_string());
                walk(sub, words, out);
                words.pop();
            }
        }
        let mut out = Vec::new();
        walk(&crate::Cli::command(), &mut Vec::new(), &mut out);
        out
    }

    fn named(words: &[String], cli: &[&str]) -> bool {
        words.iter().map(String::as_str).eq(cli.iter().copied())
    }

    #[test]
    fn every_json_command_is_an_operation_or_says_why_not() {
        let commands = commands();
        let mut unlisted = Vec::new();
        for (words, _) in commands.iter().filter(|(_, json)| *json) {
            let w: Vec<&str> = words.iter().map(String::as_str).collect();
            let ops = OPS.iter().filter(|op| op.cli == w.as_slice()).count();
            match (ops, not_an_op(&w)) {
                (1, None) | (0, Some(_)) => {}
                (n, why) => unlisted.push(format!("{w:?}: {n} operations, excluded: {why:?}")),
            }
        }
        assert_eq!(unlisted, Vec::<String>::new());
        // Nothing listed is stale: each operation is a `--json` command,
        // and each exclusion names a command.
        for op in OPS {
            assert!(
                commands
                    .iter()
                    .any(|(words, json)| *json && named(words, op.cli)),
                "{} names no --json command",
                op.name
            );
        }
        for not in NOT_OPS {
            assert!(
                commands.iter().any(|(words, _)| named(words, not.cli)),
                "{:?} names no command",
                not.cli
            );
        }
    }
}
