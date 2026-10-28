//! Mail and leases through the mdkg CLI's JSON contract (the M1 draft:
//! `msg inbox`, `msg wait`, `msg import`, `msg ack`, `claim`, `release`).
//!
//! The launcher reads and acks as the agent (`--as <agent>`), since it is
//! the agent's runtime, and imports as itself, so the reply's `via` names
//! the launcher.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::error::{CODE_MAIL_FAILED, ErrorKind, RunError};

/// One message as `msg inbox` prints it. Fields the launcher doesn't use
/// are ignored, so the contract can grow.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub message_id: String,
    pub seq: u64,
    #[serde(default)]
    pub context_id: Option<String>,
    pub kind: String,
    pub sender: String,
    #[serde(default)]
    pub via: Option<String>,
    #[serde(default)]
    pub room: Option<String>,
    #[serde(default)]
    pub sent_at: Option<String>,
    /// Absent once the body is purged.
    #[serde(default)]
    pub parts: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Inbox {
    #[serde(default)]
    pub messages: Vec<Message>,
    /// The highest `seq` returned.
    #[serde(default)]
    pub cursor: Option<u64>,
    /// More unacked mail than `--limit` returned.
    #[serde(default)]
    pub more: bool,
}

/// Receiver reads deliberately deserialize only mail headers, never bodies.
#[derive(Debug, Clone, Deserialize)]
pub struct Header {
    pub seq: u64,
    pub kind: String,
    pub sender: String,
    pub room: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Headers {
    pub messages: Vec<Header>,
    pub cursor: Option<u64>,
    pub more: bool,
}

/// What a turn imports as its reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import<'a> {
    pub kind: &'a str,
    /// The agent the reply is from.
    pub from: &'a str,
    pub reply_to: &'a str,
    /// The sender to reply to, for a direct message.
    pub to: &'a str,
    /// The room to reply into, for a room message (instead of `to`).
    pub room: Option<&'a str>,
    pub context: Option<&'a str>,
    pub file: &'a Path,
}

/// A note the launcher sends as itself, never as the agent: what it did
/// with the agent's turn (audit, 2026-10-22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note<'a> {
    /// The sender it goes to, directly: a room post's sender too.
    pub to: &'a str,
    /// The agent's reply it answers, in that reply's conversation; none for
    /// a reply that went into a room the launcher may not be in.
    pub reply_to: Option<&'a str>,
    pub text: &'a str,
}

/// The coordinator calls a turn makes.
pub trait Mail {
    /// Durable coordinator metadata for decisions already retained in this
    /// turn's inbox. Old/mocked clients promote nothing by default.
    fn decision_audit(&mut self, _agent: &str, _ids: &[String]) -> Result<Vec<Value>, RunError> {
        Ok(Vec::new())
    }
    /// A no-mark header page, oldest first; no fallback to a body listing.
    fn headers(
        &mut self,
        _agent: &str,
        _after: Option<u64>,
        _limit: usize,
    ) -> Result<Headers, RunError> {
        Err(RunError::failure(
            CODE_MAIL_FAILED,
            "mail client has no no-mark header read",
        ))
    }
    fn wait_headers(
        &mut self,
        _agent: &str,
        _after: Option<u64>,
        _timeout_secs: u64,
    ) -> Result<Option<Headers>, RunError> {
        Err(RunError::failure(
            CODE_MAIL_FAILED,
            "mail client has no no-mark header wait",
        ))
    }
    /// The agent's unacked mail, oldest first.
    fn inbox(&mut self, agent: &str, limit: usize) -> Result<Inbox, RunError>;
    /// The agent's unacked mail newer than `after`, oldest first: the next
    /// page after an inbox's `cursor`.
    fn inbox_after(
        &mut self,
        agent: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Inbox, RunError> {
        let Some(after) = after else {
            return self.inbox(agent, limit);
        };
        let all = self.inbox(agent, usize::MAX)?;
        let newer: Vec<Message> = all.messages.into_iter().filter(|m| m.seq > after).collect();
        let more = newer.len() > limit;
        let messages: Vec<Message> = newer.into_iter().take(limit).collect();
        Ok(Inbox {
            cursor: messages.iter().map(|m| m.seq).max(),
            messages,
            more,
        })
    }
    /// Blocks until unacked mail newer than `after` exists; `None` on
    /// timeout.
    fn wait(
        &mut self,
        agent: &str,
        after: Option<u64>,
        timeout_secs: u64,
    ) -> Result<Option<Inbox>, RunError>;
    /// Records the reply; returns its message id.
    fn import(&mut self, import: &Import<'_>) -> Result<String, RunError>;
    fn ack(&mut self, agent: &str, ids: &[String], outcome: &str) -> Result<(), RunError>;
    /// Takes the lease on a scope.
    fn claim(&mut self, scope: &str) -> Result<(), RunError>;
    /// Renews this holder's lease on a scope mid-turn, or takes it again
    /// if it lapsed and nobody else has it: a claim of a turn one already
    /// holds is refused (round 6).
    fn renew(&mut self, scope: &str) -> Result<(), RunError> {
        self.claim(scope)
    }
    /// Renew an already-held turn, never reacquiring one that expired.
    /// Returns the verified deadline (epoch milliseconds). Receiver admission
    /// must abort if continuity cannot be verified.
    fn renew_held(&mut self, _scope: &str) -> Result<u64, RunError> {
        Err(RunError::failure(
            CODE_MAIL_FAILED,
            "mail client cannot verify continuous turn ownership",
        ))
    }
    fn release(&mut self, scope: &str) -> Result<(), RunError>;
    /// Who claims: unique to this launcher process.
    fn holder(&self) -> String;
    /// The actor that imports replies (the launcher's own): a send skips
    /// its writer, so a reply can't go to it.
    fn writer(&self) -> Option<String> {
        None
    }
    /// Releases a lease a dead launcher process left, as its holder.
    fn release_as(&mut self, scope: &str, holder: &str) -> Result<(), RunError>;
    /// Whether `actor` holds an active lease covering `scope`, a path
    /// relative to the repository's top (`holds`): before the launcher
    /// commits a worker's change under `.mdkg/` (audit, 2026-10-22).
    /// With `node`, the node the file is: held only if nobody holds it
    /// under another path.
    fn holds(&mut self, actor: &str, scope: &str, node: Option<&str>) -> Result<bool, RunError>;
    /// Sends a note from the launcher's own actor; returns its message id.
    fn note(&mut self, note: &Note<'_>) -> Result<String, RunError>;
}

/// [`Mail`] through the mdkg CLI.
#[derive(Debug, Clone)]
pub struct CliMail {
    /// The mdkg binary: `$MDKG_RUN_MDKG`, else the given default
    /// (usually the running executable).
    pub program: PathBuf,
    pub root: PathBuf,
    /// The launcher's own actor, used for imports.
    pub launcher: String,
    /// Who holds the turn lease: unique to this process
    /// (`<launcher>@run-<uuid>`), so two launchers running as the same
    /// actor can't both renew it.
    pub holder: String,
    /// The git common dir the launcher trusts, passed to every mdkg it
    /// runs ($MDKG_TRUSTED_COMMON_DIR).
    pub trusted_common: Option<PathBuf>,
    /// The worktree `root` is in, which every mdkg it runs must find
    /// there ($MDKG_TRUSTED_WORKTREE).
    pub trusted_worktree: Option<PathBuf>,
}

/// `wait` ran out of time (the M1 draft's exit 6, question Q4).
const EXIT_WAIT_TIMEOUT: i32 = 6;

// One shared cap for holds, the local heartbeat and its verification, not
// five seconds per subprocess. Initial lookup does not yet know the expiry.
const RECEIVER_RENEWAL_BUDGET: Duration = Duration::from_secs(5);

fn renewal_deadline(
    cap: Instant,
    lease_deadline_ms: u64,
    wall_now_ms: u64,
    mono_now: Instant,
) -> Option<Instant> {
    let remaining = lease_deadline_ms.checked_sub(wall_now_ms)?;
    if remaining == 0 {
        return None;
    }
    // Retain half the old lease as slack for admission/cleanup. This is a
    // wait budget, never a substitute for the dispatcher's epoch check.
    Some(cap.min(mono_now.checked_add(Duration::from_millis(remaining / 2))?))
}

impl CliMail {
    pub fn new(default_program: PathBuf, root: PathBuf, launcher: String) -> Self {
        let program = std::env::var_os("MDKG_RUN_MDKG")
            .filter(|v| !v.is_empty())
            .map_or(default_program, PathBuf::from);
        let holder = format!("{launcher}@run-{}", uuid::Uuid::now_v7().simple());
        Self {
            program,
            root,
            launcher,
            holder,
            trusted_common: None,
            trusted_worktree: None,
        }
    }

    fn header_call(
        &self,
        agent: &str,
        event: &str,
        after: Option<u64>,
        limit: usize,
        timeout: u64,
    ) -> Result<Option<Headers>, RunError> {
        // A reminder names at most five messages. Keep pages small even
        // when headless batch policy is larger: no-mark JSON still carries
        // bodies, which this typed decoder ignores. Allow pretty-printed
        // expansion of valid nested 64 KiB bodies without unbounded reads.
        let limit = limit.min(5);
        #[derive(Deserialize)]
        struct Diagnostic {
            code: String,
        }
        #[derive(Deserialize)]
        struct Envelope {
            ok: bool,
            data: Option<Headers>,
            #[serde(default)]
            diagnostics: Vec<Diagnostic>,
        }
        let mut command = Command::new(&self.program);
        if let Some(c) = &self.trusted_common {
            command.env(crate::profile::TRUSTED_COMMON_DIR, c);
        }
        if let Some(w) = &self.trusted_worktree {
            command.env(crate::profile::TRUSTED_WORKTREE, w);
        }
        if mdkg_coord_sqlite::store_upgrades_allowed() {
            command.arg("--allow-store-upgrade");
        }
        command
            .arg("--root")
            .arg(&self.root)
            .arg(format!("--as={agent}"))
            .args(["msg", event, "--no-mark", "--room-kind"])
            .arg(crate::hooks::ROOM_WAKE_KINDS.join(","))
            .arg("--limit")
            .arg(limit.to_string())
            .arg("--json");
        if let Some(after) = after {
            command.arg("--after").arg(after.to_string());
        }
        if event == "wait" {
            command.arg("--timeout").arg(timeout.to_string());
        }
        let out = crate::turn::capped_output(
            &mut command,
            64 << 20,
            std::time::Duration::from_secs(timeout.saturating_add(10)),
        )
        .map_err(|_| RunError::failure(CODE_MAIL_FAILED, "receiver header read failed"))?;
        let envelope: Envelope = serde_json::from_slice(&out.stdout).map_err(|_| {
            RunError::failure(
                CODE_MAIL_FAILED,
                "receiver header read returned an invalid envelope",
            )
        })?;
        if envelope.ok && out.status.success() {
            return envelope.data.map(Some).ok_or_else(|| {
                RunError::failure(CODE_MAIL_FAILED, "receiver header page is missing")
            });
        }
        if event == "wait" && envelope.diagnostics.iter().any(|d| d.code == WAIT_TIMEOUT) {
            return Ok(None);
        }
        Err(RunError::failure(
            CODE_MAIL_FAILED,
            "receiver header read was refused",
        ))
    }

    /// When this holder's lease on a turn ends, or `None` when it holds
    /// none: `holds` answers a turn from the local store alone.
    fn turn_deadline(&self, scope: &str) -> Result<Option<u64>, RunError> {
        self.turn_deadline_until(scope, None)
    }

    fn turn_deadline_until(
        &self,
        scope: &str,
        until: Option<Instant>,
    ) -> Result<Option<u64>, RunError> {
        let (_, data) = self.call_until(&self.holder, &["holds", "--", scope], until)?;
        Ok(if data["holds"] == true {
            data["lease"]["deadline_ms"].as_u64()
        } else {
            None
        })
    }

    /// Runs one command and returns its envelope's `data`, or the first
    /// error diagnostic as a [`RunError`].
    fn call(&self, actor: &str, args: &[&str]) -> Result<(i32, Value), RunError> {
        self.call_until(actor, args, None)
    }

    fn call_until(
        &self,
        actor: &str,
        args: &[&str],
        until: Option<Instant>,
    ) -> Result<(i32, Value), RunError> {
        // `--json` goes before any `--`, after which everything is a value.
        let split = args.iter().position(|a| *a == "--").unwrap_or(args.len());
        let mut cmd = Command::new(&self.program);
        if let Some(c) = &self.trusted_common {
            cmd.env(crate::profile::TRUSTED_COMMON_DIR, c);
        }
        if let Some(w) = &self.trusted_worktree {
            cmd.env(crate::profile::TRUSTED_WORKTREE, w);
        }
        // A launcher allowed to upgrade the store passes that on (lane 2's
        // guard): its own mail calls run the same build.
        if mdkg_coord_sqlite::store_upgrades_allowed() {
            cmd.arg("--allow-store-upgrade");
        }
        cmd.arg("--root")
            .arg(&self.root)
            .arg(format!("--as={actor}"))
            .args(&args[..split])
            .arg("--json")
            .args(&args[split..]);
        let out = match until {
            Some(until) => bounded_output(&mut cmd, until),
            None => cmd.output(),
        }
        .map_err(|e| {
            RunError::failure(
                CODE_MAIL_FAILED,
                format!("can't run {}: {e}", self.program.display()),
            )
        })?;
        let code = out.status.code().unwrap_or(-1);
        let envelope: Value = serde_json::from_slice(&out.stdout).map_err(|_| {
            RunError::failure(
                CODE_MAIL_FAILED,
                format!(
                    "`{}` didn't print the JSON contract (exit {code})",
                    args.first().copied().unwrap_or_default()
                ),
            )
            .with_hint("the messaging commands (M1) may not be in this mdkg build")
        })?;
        if envelope.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok((code, envelope.get("data").cloned().unwrap_or(Value::Null)));
        }
        let diag = envelope
            .get("diagnostics")
            .and_then(Value::as_array)
            .and_then(|d| {
                d.iter()
                    .find(|x| x.get("severity").and_then(Value::as_str) == Some("error"))
                    .or_else(|| d.first())
            });
        let text = |k: &str| diag.and_then(|d| d.get(k)).and_then(Value::as_str);
        let mut e = RunError::new(
            ErrorKind::from_exit(code),
            text("code").unwrap_or(CODE_MAIL_FAILED),
            text("message").unwrap_or("the mail command failed"),
        );
        e.hint = text("hint").map(String::from);
        if code == EXIT_WAIT_TIMEOUT {
            e.code = WAIT_TIMEOUT.to_string();
        }
        Err(e)
    }

    fn inbox_from(data: Value) -> Result<Inbox, RunError> {
        serde_json::from_value(data).map_err(|e| {
            RunError::failure(CODE_MAIL_FAILED, format!("unexpected inbox output: {e}"))
        })
    }
}

const WAIT_TIMEOUT: &str = "msg.wait_timeout";

impl Mail for CliMail {
    fn decision_audit(&mut self, agent: &str, ids: &[String]) -> Result<Vec<Value>, RunError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let flags: Vec<String> = ids.iter().map(|id| format!("--message={id}")).collect();
        let mut args = vec!["msg", "decisions"];
        args.extend(flags.iter().map(String::as_str));
        let (_, data) = self.call(agent, &args)?;
        data.get("decisions")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| RunError::failure(CODE_MAIL_FAILED, "unexpected decision audit output"))
    }
    fn headers(
        &mut self,
        agent: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Headers, RunError> {
        self.header_call(agent, "inbox", after, limit, 0)?
            .ok_or_else(|| RunError::failure(CODE_MAIL_FAILED, "missing header page"))
    }
    fn wait_headers(
        &mut self,
        agent: &str,
        after: Option<u64>,
        timeout_secs: u64,
    ) -> Result<Option<Headers>, RunError> {
        self.header_call(agent, "wait", after, 200, timeout_secs)
    }
    fn inbox(&mut self, agent: &str, limit: usize) -> Result<Inbox, RunError> {
        let limit = limit.to_string();
        let (_, data) = self.call(agent, &["msg", "inbox", "--limit", &limit])?;
        Self::inbox_from(data)
    }

    fn inbox_after(
        &mut self,
        agent: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Inbox, RunError> {
        let limit = limit.to_string();
        let after = after.map(|a| a.to_string());
        let mut args = vec!["msg", "inbox", "--limit", &limit];
        if let Some(a) = &after {
            args.extend(["--after", a]);
        }
        let (_, data) = self.call(agent, &args)?;
        Self::inbox_from(data)
    }

    fn wait(
        &mut self,
        agent: &str,
        after: Option<u64>,
        timeout_secs: u64,
    ) -> Result<Option<Inbox>, RunError> {
        let timeout = timeout_secs.to_string();
        let after_s = after.map(|a| a.to_string());
        let mut args = vec!["msg", "wait", "--timeout", &timeout];
        if let Some(a) = &after_s {
            args.extend(["--after", a]);
        }
        match self.call(agent, &args) {
            Ok((_, data)) => Self::inbox_from(data).map(Some),
            Err(e) if e.code == WAIT_TIMEOUT => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn import(&mut self, i: &Import<'_>) -> Result<String, RunError> {
        // Ids and names come from other agents: `--flag=value` keeps one that
        // starts with `-` from parsing as a flag, and `--to` splits on
        // commas, so a comma in a name would add recipients.
        if i.room.is_none() && i.to.contains(',') {
            return Err(RunError::failure(
                CODE_MAIL_FAILED,
                "the sender's name contains a comma, so the reply can't be addressed to it alone",
            ));
        }
        let mut args = vec![
            "msg".to_string(),
            "import".into(),
            format!("--kind={}", i.kind),
            format!("--from={}", i.from),
            format!("--reply-to={}", i.reply_to),
            match i.room {
                Some(r) => format!("--room={r}"),
                None => format!("--to={}", i.to),
            },
        ];
        if let Some(c) = i.context {
            args.push(format!("--context={c}"));
        }
        args.push("--".into());
        args.push(i.file.display().to_string());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let launcher = self.launcher.clone();
        let (_, data) = self.call(&launcher, &args)?;
        data.pointer("/message/messageId")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| {
                RunError::failure(CODE_MAIL_FAILED, "msg import didn't return a messageId")
            })
    }

    fn ack(&mut self, agent: &str, ids: &[String], outcome: &str) -> Result<(), RunError> {
        let outcome = format!("--outcome={outcome}");
        let mut args = vec!["msg", "ack", &outcome, "--"];
        args.extend(ids.iter().map(String::as_str));
        self.call(agent, &args).map(|_| ())
    }

    fn claim(&mut self, scope: &str) -> Result<(), RunError> {
        let holder = self.holder.clone();
        self.call(&holder, &["claim", scope]).map(|_| ())
    }

    fn renew(&mut self, scope: &str) -> Result<(), RunError> {
        // The holder is this process's alone, so `renew` extends the turn
        // and nothing else.
        let holder = self.holder.clone();
        // Not needed for `renew` itself, which works without main's ref
        // beside an authority; only to read a failure (verification of
        // ec063a4).
        let before = self.turn_deadline(scope).ok().flatten();
        match self.call(&holder, &["renew"]) {
            Ok((_, data)) => {
                let renewed = data["renewed"]
                    .as_array()
                    .is_some_and(|l| l.iter().any(|r| r["scope"] == scope));
                if renewed {
                    return Ok(());
                }
            }
            // A turn is renewed here and never handed to a lease
            // authority, but `renew` fails when the authority does, with
            // whatever exit it gave: the turn's own deadline says whether
            // it moved (verification of 4f92eb3 and ec063a4). One held by
            // someone else has none here, and claiming it is refused.
            Err(e) => match (before, self.turn_deadline(scope)) {
                (Some(b), Ok(Some(a))) if a > b => return Ok(()),
                (_, Ok(None)) => {}
                _ => return Err(e),
            },
        }
        self.claim(scope)
    }

    fn renew_held(&mut self, scope: &str) -> Result<u64, RunError> {
        let lost = || {
            RunError::failure(
                CODE_MAIL_FAILED,
                "receiver turn lease expired or changed holder",
            )
        };
        let cap = Instant::now() + RECEIVER_RENEWAL_BUDGET;
        let before = self
            .turn_deadline_until(scope, Some(cap))?
            .ok_or_else(lost)?;
        let holder = self.holder.clone();
        // Never let renewal or its verification wait past the current lease.
        // This path never calls claim, even if the holder disappeared.
        let until = renewal_deadline(cap, before, crate::state::now_ms(), Instant::now())
            .ok_or_else(lost)?;
        let renewed = self.call_until(&holder, &["renew", "--local-turn", scope], Some(until));
        let after = self
            .turn_deadline_until(scope, Some(until))?
            .ok_or_else(lost)?;
        if after <= crate::state::now_ms() || Instant::now() >= until {
            return Err(lost());
        }
        match renewed {
            Ok((_, data))
                if data["renewed"]
                    .as_array()
                    .is_some_and(|l| l.iter().any(|r| r["scope"] == scope)) =>
            {
                Ok(after)
            }
            // A command can fail after its local heartbeat persisted.
            Err(_) if after > before => Ok(after),
            Err(e) => Err(e),
            _ => Err(lost()),
        }
    }

    fn release(&mut self, scope: &str) -> Result<(), RunError> {
        let holder = self.holder.clone();
        self.call(&holder, &["release", scope]).map(|_| ())
    }

    fn holder(&self) -> String {
        self.holder.clone()
    }

    fn writer(&self) -> Option<String> {
        Some(self.launcher.clone())
    }

    fn release_as(&mut self, scope: &str, holder: &str) -> Result<(), RunError> {
        self.call(holder, &["release", scope]).map(|_| ())
    }

    fn note(&mut self, n: &Note<'_>) -> Result<String, RunError> {
        if n.to.contains(',') {
            return Err(RunError::failure(
                CODE_MAIL_FAILED,
                "the sender's name contains a comma, so a note can't be addressed to it alone",
            ));
        }
        let mut args = vec![
            "msg".to_string(),
            "send".into(),
            "--kind=note".into(),
            format!("--to={}", n.to),
            format!("--text={}", n.text),
        ];
        if let Some(r) = n.reply_to {
            args.push(format!("--reply-to={r}"));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let launcher = self.launcher.clone();
        let (_, data) = self.call(&launcher, &args)?;
        data.pointer("/message/messageId")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| {
                RunError::failure(CODE_MAIL_FAILED, "msg send didn't return a messageId")
            })
    }

    fn holds(&mut self, actor: &str, scope: &str, node: Option<&str>) -> Result<bool, RunError> {
        // After `--`: a path the agent named can't parse as a flag.
        let node = node.map(|n| format!("--node={n}"));
        let mut args = vec!["holds"];
        args.extend(node.as_deref());
        args.extend(["--", scope]);
        let (_, data) = self.call(actor, &args)?;
        data.get("holds").and_then(Value::as_bool).ok_or_else(|| {
            RunError::failure(
                CODE_MAIL_FAILED,
                "holds didn't say whether the lease is held",
            )
        })
    }
}

/// Lease calls have a wall-clock bound independent of a configured authority.
/// Anonymous files avoid pipe backpressure or descendants holding a pipe open.
fn bounded_output(cmd: &mut Command, until: Instant) -> std::io::Result<std::process::Output> {
    use std::{
        fs,
        io::{Read, Seek},
        os::unix::{fs::OpenOptionsExt, process::CommandExt},
        process::Stdio,
    };
    let file = || -> std::io::Result<fs::File> {
        let path = std::env::temp_dir().join(format!("mdkg-lease-call-{}", uuid::Uuid::now_v7()));
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        fs::remove_file(path)?;
        Ok(f)
    };
    if Instant::now() >= until {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "turn lease check timed out",
        ));
    }
    let mut stdout = file()?;
    let mut stderr = file()?;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .process_group(0)
        .spawn()?;
    let status = loop {
        let poll = child.try_wait();
        if let Ok(Some(status)) = poll {
            break Ok(status);
        }
        if poll.is_err()
            || Instant::now() >= until
            || stdout.metadata().map_or(true, |m| m.len() > 1 << 20)
            || stderr.metadata().map_or(true, |m| m.len() > 1 << 20)
        {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", child.id())])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = child.kill();
            let _ = child.wait();
            break Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "turn lease check timed out or exceeded its output limit",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }?;
    let read = |file: &mut fs::File| -> std::io::Result<Vec<u8>> {
        file.rewind()?;
        let mut bytes = Vec::new();
        file.take((1 << 20) + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 1 << 20 {
            return Err(std::io::Error::other("turn lease output exceeds limit"));
        }
        Ok(bytes)
    };
    Ok(std::process::Output {
        status,
        stdout: read(&mut stdout)?,
        stderr: read(&mut stderr)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in mdkg: `holds` answers the next of `deadlines` (a
    /// number, or `none`), `renew` fails as a lease authority's outage
    /// does, `claim` succeeds; each call is logged.
    fn fake_mdkg(name: &str, deadlines: &[&str]) -> (PathBuf, CliMail) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mdkg-mail-{name}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("deadlines"), deadlines.join("\n") + "\n").expect("w");
        let script = dir.join("mdkg");
        std::fs::write(
            &script,
            r#"#!/bin/sh
d=$(dirname "$0")
for a; do case $a in holds|renew|claim) cmd=$a; break;; esac; done
echo "$cmd" >> "$d/calls"
case $cmd in
holds)
  n=$(cat "$d/n" 2>/dev/null || echo 0); echo $((n + 1)) > "$d/n"
  dl=$(sed -n "$((n + 1))p" "$d/deadlines")
  if [ "$dl" = none ]; then echo '{"ok":true,"data":{"holds":false,"lease":null}}'
  elif [ "$dl" = fail ]; then
    echo '{"ok":false,"diagnostics":[{"severity":"error","code":"coord.main_missing","message":"no main"}]}'
    exit 1
  else echo "{\"ok\":true,\"data\":{\"holds\":true,\"lease\":{\"deadline_ms\":$dl}}}"; fi;;
renew)
  if [ -f "$d/renew_delay" ]; then /bin/sleep "$(cat "$d/renew_delay")"; fi
  if [ -f "$d/renew_block" ]; then
    /bin/sleep 10
    printf completed > "$d/renew_completed"
  fi
  if [ -f "$d/renew_ok" ]; then
    echo '{"ok":true,"data":{"renewed":[{"scope":"run:x"}]}}'; exit 0
  fi
  echo '{"ok":false,"diagnostics":[{"severity":"error","code":"coord.authority_failed","message":"down"}]}'
  exit "$(cat "$d/renew_exit" 2>/dev/null || echo 4)";;
claim) echo '{"ok":true,"data":{}}';;
esac
"#,
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        // Let macOS assess the freshly written executable before tests of
        // the receiver's shared command deadline. No fixture deadline is
        // consumed by this no-op, and production's deadline stays unchanged.
        Command::new(&script)
            .arg("warmup")
            .output()
            .expect("warm fixture");
        std::fs::remove_file(dir.join("calls")).expect("clear warmup call");
        let mail = CliMail::new(script, dir.clone(), "launcher".into());
        (dir, mail)
    }

    /// Verification of 4f92eb3: a lease authority's outage fails `renew`
    /// after the turn was renewed here; that counts, and a turn that
    /// didn't move, or lapsed, is handled as such.
    #[test]
    fn a_turn_renewed_here_counts_though_the_authority_failed() {
        let calls = |dir: &Path| std::fs::read_to_string(dir.join("calls")).unwrap_or_default();
        let (dir, mut mail) = fake_mdkg("moved", &["100", "200"]);
        mail.renew("run:x").expect("renewed here");
        assert_eq!(calls(&dir), "holds\nrenew\nholds\n");
        let _ = std::fs::remove_dir_all(dir);
        let (dir, mut mail) = fake_mdkg("stuck", &["100", "100"]);
        let e = mail.renew("run:x").expect_err("not renewed");
        assert_eq!(e.code, "coord.authority_failed");
        assert_eq!(calls(&dir), "holds\nrenew\nholds\n");
        let _ = std::fs::remove_dir_all(dir);
        let (dir, mut mail) = fake_mdkg("lapsed", &["none", "none"]);
        mail.renew("run:x").expect("taken again");
        assert_eq!(calls(&dir), "holds\nrenew\nholds\nclaim\n");
        let _ = std::fs::remove_dir_all(dir);
        // An authority's failure with a Conflict exit is read the same way
        // (verification of ec063a4).
        let (dir, mut mail) = fake_mdkg("conflict", &["100", "200"]);
        std::fs::write(dir.join("renew_exit"), "5").expect("w");
        mail.renew("run:x").expect("renewed here");
        let _ = std::fs::remove_dir_all(dir);
        // `holds` failing (main's ref gone) doesn't stop a renewal that
        // works.
        let (dir, mut mail) = fake_mdkg("nomain", &["fail"]);
        std::fs::write(dir.join("renew_ok"), "").expect("w");
        mail.renew("run:x").expect("renewed");
        assert_eq!(calls(&dir), "holds\nrenew\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn receiver_renewal_never_reclaims_expired_or_replaced_lease() {
        for deadlines in [
            vec!["none"],
            vec!["1"],
            vec!["9999999999999", "none"],
            vec!["9999999999999", "1"],
        ] {
            let (dir, mut mail) = fake_mdkg("receiver-lost", &deadlines);
            std::fs::write(dir.join("renew_ok"), "").unwrap();
            assert!(mail.renew_held("run:x").is_err());
            let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
            assert!(!calls.contains("claim"), "{calls}");
            std::fs::remove_dir_all(dir).unwrap();
        }
        let (dir, mut mail) = fake_mdkg("receiver-owned", &["9999999999999", "9999999999999"]);
        std::fs::write(dir.join("renew_ok"), "").unwrap();
        mail.renew_held("run:x").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("calls")).unwrap(),
            "holds\nrenew\nholds\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn receiver_renewal_deadline_shares_five_seconds_and_retains_half_the_lease() {
        let start = Instant::now();
        let cap = start + RECEIVER_RENEWAL_BUDGET;
        assert_eq!(RECEIVER_RENEWAL_BUDGET, Duration::from_secs(5));
        assert_eq!(renewal_deadline(cap, 21_000, 1_000, start), Some(cap));
        assert_eq!(
            renewal_deadline(cap, 5_000, 1_000, start),
            Some(start + Duration::from_secs(2))
        );
        // Three seconds used by initial lookup are not granted again.
        assert_eq!(
            renewal_deadline(cap, 21_000, 4_000, start + Duration::from_secs(3)),
            Some(cap)
        );
        // Nor does a stopped supervisor get a fresh relative countdown.
        assert_eq!(
            renewal_deadline(cap, 21_000, 7_000, start + Duration::from_secs(6)),
            Some(cap)
        );
        assert_eq!(renewal_deadline(cap, 1_000, 1_000, start), None);
        assert_eq!(renewal_deadline(cap, 999, 1_000, start), None);
    }

    #[test]
    fn receiver_renewal_tolerates_a_local_call_slower_than_one_second() {
        let (dir, mut mail) = fake_mdkg("receiver-slow", &["9999999999999", "9999999999999"]);
        std::fs::write(dir.join("renew_delay"), "1.2").unwrap();
        std::fs::write(dir.join("renew_ok"), "").unwrap();
        let result = mail.renew_held("run:x");
        std::fs::remove_dir_all(dir).unwrap();
        result.expect("a slow local check still fits the bounded renewal budget");
    }

    #[test]
    fn receiver_renewal_is_bounded_when_local_command_blocks() {
        let (dir, mut mail) = fake_mdkg("receiver-blocked", &["9999999999999", "9999999999999"]);
        std::fs::write(dir.join("renew_block"), "").unwrap();
        std::fs::write(dir.join("renew_ok"), "").unwrap();
        assert!(mail.renew_held("run:x").is_err());
        let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
        assert!(
            calls.contains("renew"),
            "the blocking call was reached: {calls}"
        );
        assert!(!calls.contains("claim"));
        assert!(
            !dir.join("renew_completed").exists(),
            "the bounded check must stop the blocked command, not merely reject its late reply"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_inbox_parses_the_draft_contract_and_ignores_extra_fields() {
        let data = serde_json::json!({
            "mailboxes": ["go-lane-b"],
            "messages": [{
                "messageId": "m1", "seq": 412, "contextId": "room:review",
                "kind": "findings", "sender": "codex:pr19", "via": "orchestrator",
                "room": "review", "to": ["go-lane-b"], "referenceIds": [],
                "parts": [{"kind": "text", "text": "hi"}],
                "body": "present", "digest": "sha256:x", "size": 2,
                "retention": "purge_on_ack", "sentAt": "2026-10-22T14:03:11Z",
                "sentAtMs": 1, "deliveries": []
            }],
            "cursor": 412, "more": false
        });
        let inbox = CliMail::inbox_from(data).expect("parses");
        assert_eq!(inbox.cursor, Some(412));
        assert_eq!(inbox.messages[0].sender, "codex:pr19");
        assert_eq!(inbox.messages[0].parts.len(), 1);
    }

    #[test]
    fn a_purged_message_has_no_parts() {
        let data = serde_json::json!({"messages": [{
            "messageId": "m1", "seq": 1, "kind": "note", "sender": "a", "body": "purged"
        }]});
        let inbox = CliMail::inbox_from(data).expect("parses");
        assert!(inbox.messages[0].parts.is_empty());
    }
}
