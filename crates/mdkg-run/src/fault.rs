//! Faults (a-flow A26): a condition the launcher mails to an agent's
//! `reports_to` as kind `fault`, from `mdkg:supervisor`, so a failed turn
//! is never silent.
//!
//! A fault survives a crash and one failed write at every step:
//!
//! - its id comes from what it reports ([`fault_id`]), never a fresh random
//!   number, so any resend from any path is the same occurrence, and the
//!   mail store keeps its first copy;
//! - before sending, the launcher writes it to a pending file under
//!   `<run>/faults/<agent>/` ([`write_pending`]), deleted once the send
//!   succeeds and sent again at every loop boundary and start ([`resend`]);
//! - a fault whose pending file couldn't be written is derived again from
//!   its source: a turn's error from the turn's retained receipt
//!   ([`rederive`]).
//!
//! Fault text never quotes mail bodies, harness output or matched secrets:
//! the message is the code's own summary, and the worktree is a count of
//! changed paths, never their names.

use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use mdkg_format::program::BIN;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::RunError;
use crate::install::at::{self, Links, Tree};
use crate::mail::{FaultSent, Mail};
use crate::state;

/// A fault's data part.
pub const SCHEMA: &str = "mdkg.run.fault.v1";

/// The sender of every fault, a reserved actor.
pub const SENDER: &str = "mdkg:supervisor";

/// The folder under the launcher's run directory that holds pending faults.
const FOLDER: &str = "faults";

/// The most bytes a pending file may hold.
const MAX_PENDING_BYTES: u64 = 64 << 10;

/// The most retained receipts [`rederive`] reads, newest first.
const REDERIVE_RECEIPTS: usize = 50;

/// What a turn-error fault reports as cleared by (A26's table).
pub const CLEARS_TURN: &str = "the agent's next turn ends without an error";

/// What a refused reply's fault reports as cleared by (A26's table).
pub const CLEARS_REPLY: &str = "the agent's next reply imports";

/// Whether a turn's error is its reply refused by the mail secret filter or
/// size cap: a fault to the reply's author as well (A26's table).
pub fn refused_reply(code: &str) -> bool {
    matches!(code, "msg.secret_refused" | "msg.too_large")
}

/// The agent's worktree, as a fault reports it: whether it holds
/// uncommitted changes, and how many paths, never which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    pub path: String,
    pub dirty: bool,
    pub changed: u64,
}

impl Worktree {
    /// `top`'s state from a bounded `git status`; `None` when it can't be
    /// read.
    pub fn of(top: &Path) -> Option<Self> {
        let changed = crate::hooks::changed_paths(top).ok()?.len() as u64;
        Some(Self {
            path: top.display().to_string(),
            dirty: changed > 0,
            changed,
        })
    }
}

/// One fault occurrence, before the mail store numbers its copy and
/// carries the count on. It is also the pending file's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fault {
    pub fault_id: String,
    pub agent: String,
    pub code: String,
    /// The code's own summary, never an error's free text.
    pub message: String,
    pub turn_id: Option<String>,
    pub supervisor_id: Option<String>,
    /// The nodes the agent holds leases on.
    pub nodes: Vec<String>,
    pub worktree: Option<Worktree>,
    /// What clears it (A26's table).
    pub clears: String,
    /// One line on what to do.
    pub suggested: String,
    /// Its recipients: the agent's `reports_to`, and for some sources the
    /// agent too.
    pub to: Vec<String>,
}

/// The occurrence's id: a UUID (version 8, the RFC 9562 custom layout)
/// from SHA-256 over mdkg's namespace and `<agent>\n<code>\n<occurrence>`.
/// The edd names UUIDv5; this keeps the same determinism with the hash the
/// tree already carries (the orchestrator, 2026-10-30).
pub fn fault_id(agent: &str, code: &str, occurrence: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mdkg.run.fault.v1\0");
    hash.update(format!("{agent}\n{code}\n{occurrence}").as_bytes());
    let digest = hash.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// The message id of an occurrence's copy `k` (0 for the first).
pub fn message_id(fault_id: &str, k: u64) -> String {
    format!("fault.{fault_id}.r{k}")
}

/// A code's one-line summary, the only message a fault carries.
fn summary(code: &str) -> String {
    mdkg_format::Code::parse(code)
        .map(|c| c.summary().to_string())
        .unwrap_or_else(|| "The launcher reported an error with this code.".into())
}

impl Fault {
    /// A fault for `agent`'s `code` at `occurrence`, to `to`.
    pub fn new(
        agent: &str,
        code: &str,
        occurrence: &str,
        to: Vec<String>,
        clears: &str,
        suggested: String,
    ) -> Self {
        Self {
            fault_id: fault_id(agent, code, occurrence),
            agent: agent.into(),
            code: code.into(),
            message: summary(code),
            turn_id: None,
            supervisor_id: None,
            nodes: Vec::new(),
            worktree: None,
            clears: clears.into(),
            suggested,
            to,
        }
    }

    /// A turn that ended in `code`, reported to `reports_to` (R1): its
    /// occurrence is the turn. A refused reply's fault names the filter
    /// (its code's summary), never the matched text, and goes to the agent
    /// too.
    pub fn turn_error(
        agent: &state::AgentState,
        reports_to: &str,
        turn_id: &str,
        code: &str,
    ) -> Self {
        let name = &agent.spec.name;
        let (to, clears) = if refused_reply(code) {
            (vec![reports_to.to_string(), name.clone()], CLEARS_REPLY)
        } else {
            (vec![reports_to.to_string()], CLEARS_TURN)
        };
        let suggested = if code == "run.turn_interrupted" {
            format!("{BIN} agents resume {name}")
        } else if code == crate::error::CODE_LOGIN_REQUIRED {
            // The sign-in step, named for the harness: the fault's message
            // is the code's summary only.
            let harness = agent.spec.harness;
            format!(
                "sign {} in for {name}: run `{}` once in the agent's environment, then \
                 `{BIN} agents status {name}`",
                harness.as_str(),
                crate::auth::login_command(harness)
            )
        } else {
            format!("{BIN} agents status {name}")
        };
        let mut fault = Self::new(name, code, turn_id, to, clears, suggested);
        fault.turn_id = Some(turn_id.into());
        fault.worktree = Worktree::of(&agent.spec.cwd);
        fault
    }

    /// The message body: a one-line summary pointing at the data part, and
    /// the data part with the store's tally.
    pub fn parts(&self, count: u64, first_at: &str) -> Value {
        let text = format!(
            "{} fault for {}: {}\nDetails are in this message's data part ({SCHEMA}).",
            self.code, self.agent, self.message
        );
        json!([
            {"kind": "text", "text": text},
            {"kind": "data", "mediaType": "application/json", "data": {
                "schema": SCHEMA,
                "fault_id": self.fault_id,
                "agent": self.agent,
                "code": self.code,
                "message": self.message,
                "turn_id": self.turn_id,
                "supervisor_id": self.supervisor_id,
                "nodes": self.nodes,
                "worktree": self.worktree,
                "first_at": first_at,
                "count": count,
                "clears": self.clears,
                "suggested": self.suggested,
            }}
        ])
    }
}

fn folder(dir: &Path, agent: &str, create: bool) -> std::io::Result<at::Folder> {
    // dir was selected from the checked common directory, not agent text.
    let run = Tree::open(dir)?;
    let root = run.folder(Path::new(""), false, Links::Never)?;
    let tree = run.subtree(Path::new(FOLDER), create)?;
    let faults = tree.folder(Path::new(""), false, Links::Never)?;
    let agent = tree.folder(Path::new(&state::file_name(agent)), create, Links::Never)?;
    if create {
        rustix::fs::fsync(&root.fd)?;
        rustix::fs::fsync(&faults.fd)?;
    }
    Ok(agent)
}

fn check(fault: &Fault) -> Result<(), RunError> {
    state::check_name(&fault.agent)?;
    uuid::Uuid::parse_str(&fault.fault_id)
        .map_err(|_| RunError::usage(crate::error::CODE_MAIL_FAILED, "invalid fault id"))?;
    Ok(())
}

/// Where `agent`'s pending fault `fault_id` is kept.
pub fn pending_path(dir: &Path, agent: &str, fault_id: &str) -> PathBuf {
    dir.join(FOLDER)
        .join(state::file_name(agent))
        .join(format!("{fault_id}.json"))
}

/// Writes a pending fault (0600): to a temporary file, fsynced, renamed,
/// and the folder fsynced.
pub fn write_pending(dir: &Path, fault: &Fault) -> Result<(), RunError> {
    check(fault)?;
    let bytes = serde_json::to_vec(fault)
        .map_err(|e| RunError::io("can't serialize a pending fault", e.into()))?;
    let save = || -> std::io::Result<()> {
        let folder = folder(dir, &fault.agent, true)?;
        let temporary = format!(".{}.tmp", uuid::Uuid::now_v7());
        let result = (|| {
            let mut file = at::create_file(&folder.fd, OsStr::new(&temporary))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            at::rename(
                &folder.fd,
                OsStr::new(&temporary),
                &folder.fd,
                OsStr::new(&format!("{}.json", fault.fault_id)),
            )?;
            rustix::fs::fsync(&folder.fd)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = at::unlink(&folder.fd, OsStr::new(&temporary));
        }
        result
    };
    save().map_err(|e| RunError::io("can't write a pending fault", e))
}

/// Deletes a pending fault once it is in the mail store; one already gone
/// is fine.
pub fn remove_pending(dir: &Path, agent: &str, fault_id: &str) -> Result<(), RunError> {
    state::check_name(agent)?;
    let remove = || -> std::io::Result<()> {
        let folder = match folder(dir, agent, false) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            other => other?,
        };
        match at::unlink(&folder.fd, OsStr::new(&format!("{fault_id}.json"))) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            other => other?,
        }
        rustix::fs::fsync(&folder.fd)?;
        Ok(())
    };
    remove().map_err(|e| RunError::io("can't delete a sent fault's pending file", e))
}

/// `agent`'s pending faults, by file name. A file that can't be read or
/// isn't a fault of this agent is an error: never skipped as sent.
pub fn pending(dir: &Path, agent: &str) -> Result<Vec<Fault>, RunError> {
    state::check_name(agent)?;
    let read = || -> std::io::Result<Vec<Fault>> {
        let folder = match folder(dir, agent, false) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            other => other?,
        };
        let mut names: Vec<String> =
            std::fs::read_dir(pending_path(dir, agent, "x").parent().expect("folder"))?
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
                .collect();
        names.sort();
        let mut out = Vec::new();
        for name in names {
            let file = at::open_file(&folder.fd, OsStr::new(&name))?;
            let mut bytes = Vec::new();
            file.take(MAX_PENDING_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_PENDING_BYTES {
                return Err(std::io::Error::other("a pending fault exceeds its limit"));
            }
            let fault: Fault = serde_json::from_slice(&bytes)?;
            if fault.agent != agent || name != format!("{}.json", fault.fault_id) {
                return Err(std::io::Error::other(
                    "a pending fault doesn't match its file",
                ));
            }
            out.push(fault);
        }
        Ok(out)
    };
    read().map_err(|e| RunError::io("pending faults unavailable", e))
}

/// What one delivery did. A send error keeps the pending file, when it was
/// written, for the next loop boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivered {
    pub fault_id: String,
    pub code: String,
    pub sent: Option<FaultSent>,
    pub send_error: Option<RunError>,
    /// The pending file couldn't be written, or a sent one deleted.
    pub file_error: Option<RunError>,
}

fn send(mail: &mut dyn Mail, dir: &Path, fault: &Fault, written: bool) -> Delivered {
    let mut done = Delivered {
        fault_id: fault.fault_id.clone(),
        code: fault.code.clone(),
        sent: None,
        send_error: None,
        file_error: None,
    };
    match mail.fault_send(fault) {
        Ok(sent) => {
            done.sent = Some(sent);
            if written {
                // A file left by a crash here is sent again as a no-op.
                done.file_error = remove_pending(dir, &fault.agent, &fault.fault_id).err();
            }
        }
        Err(e) => done.send_error = Some(e),
    }
    done
}

/// Delivers a fault durably (A26): the pending file first, then the send,
/// then the file's deletion. A file that can't be written doesn't stop the
/// send (R42); a send that fails keeps the file (R35).
pub fn deliver(mail: &mut dyn Mail, dir: &Path, fault: &Fault) -> Delivered {
    let written = write_pending(dir, fault);
    let mut done = send(mail, dir, fault, written.is_ok());
    if let Err(e) = written {
        done.file_error = Some(e);
    }
    done
}

/// Sends `agent`'s pending faults again, under the same ids: at every loop
/// boundary and when `agents up` starts (R35).
pub fn resend(mail: &mut dyn Mail, dir: &Path, agent: &str) -> Result<Vec<Delivered>, RunError> {
    Ok(pending(dir, agent)?
        .iter()
        .map(|fault| send(mail, dir, fault, true))
        .collect())
}

/// A retained receipt's data: its turn and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub turn_id: String,
    /// `None` for a turn that ended without an error.
    pub error: Option<String>,
    pub nodes: Vec<String>,
}

/// The nodes a receipt's activity holds: those its turn claimed and still
/// holds, by id.
pub fn receipt_nodes(data: &Value) -> Vec<String> {
    let held: Vec<&Value> = data["activity"]["leases_held"]
        .as_array()
        .map(|l| l.iter().filter_map(|l| l.get("id")).collect())
        .unwrap_or_default();
    let mut nodes: Vec<String> = data["activity"]["claims"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c.get("lease").is_some_and(|l| held.contains(&l)))
        .filter_map(|c| c["node"].as_str().map(String::from))
        .collect();
    nodes.sort();
    nodes.dedup();
    nodes
}

/// How a retained receipt's turn ended, from its JSON line.
pub fn ended(body: &str) -> Option<Ended> {
    let line = body.lines().find(|l| l.starts_with('{'))?;
    let data: Value = serde_json::from_str(line).ok()?;
    if data["schema"] != "mdkg.turn.receipt.v1" {
        return None;
    }
    let error = match data["result"]["status"].as_str()? {
        "ok" => None,
        "error" => Some(data["result"]["code"].as_str()?.to_string()),
        _ => return None,
    };
    Some(Ended {
        turn_id: data["turn_id"].as_str()?.to_string(),
        error,
        nodes: receipt_nodes(&data),
    })
}

/// The agent's last turns, oldest first, from its retained receipts: at
/// most [`REDERIVE_RECEIPTS`], read without following links.
fn receipts(dir: &Path, agent: &str) -> std::io::Result<Vec<Ended>> {
    let rel = state::file_name(agent);
    let folder = match Tree::open(dir).and_then(|run| run.subtree(Path::new("receipts"), false)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other?,
    };
    let agent_folder = match folder.folder(Path::new(&rel), false, Links::Never) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other?,
    };
    // Turn ids are UUIDv7: their names sort in the order the turns began.
    let mut names: Vec<String> = std::fs::read_dir(dir.join("receipts").join(&rel))?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            n.strip_suffix(".txt")
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        })
        .collect();
    names.sort();
    let skip = names.len().saturating_sub(REDERIVE_RECEIPTS);
    let mut out = Vec::new();
    for name in &names[skip..] {
        let file = at::open_file(&agent_folder.fd, OsStr::new(name))?;
        let mut bytes = Vec::new();
        file.take(4 << 20).read_to_end(&mut bytes)?;
        if let Some(e) = ended(&String::from_utf8_lossy(&bytes))
            && name.strip_suffix(".txt") == Some(e.turn_id.as_str())
        {
            out.push(e);
        }
    }
    Ok(out)
}

/// Derives again the turn-error faults of the agent's retained receipts
/// since its last turn without an error, and sends any the store doesn't
/// hold (R42): the store keeps its first copy of an occurrence, and never
/// sends a cleared one again. Only each code's latest occurrence can be
/// open (a newer one supersedes it), so only that one is derived: a loop
/// boundary costs one send per open code, not one per failed turn.
pub fn rederive(
    mail: &mut dyn Mail,
    dir: &Path,
    agent: &state::AgentState,
) -> Result<Vec<Delivered>, RunError> {
    let ended = receipts(dir, &agent.spec.name)
        .map_err(|e| RunError::io("retained receipts unavailable", e))?;
    let start = ended
        .iter()
        .rposition(|e| e.error.is_none())
        .map_or(0, |i| i + 1);
    let latest = |i: usize, code: &str| {
        !ended[i + 1..]
            .iter()
            .any(|later| later.error.as_deref() == Some(code))
    };
    Ok(ended
        .iter()
        .enumerate()
        .skip(start)
        .filter_map(|(i, e)| {
            let code = e.error.as_deref().filter(|code| latest(i, code))?;
            let mut fault = Fault::turn_error(agent, &agent.spec.reports_to, &e.turn_id, code);
            fault.nodes = e.nodes.clone();
            Some(deliver(mail, dir, &fault))
        })
        .collect())
}

/// Clears `agent`'s faults reporting `condition` (their `clears`), now
/// seen to end: the store's open copies, and first any pending file of one
/// whose send failed, so a later resend can't reopen a condition that has
/// already ended (review of 08f0d606). A pending file that can't be read
/// or removed doesn't stop the store's clear; its error is returned after.
pub fn clear(
    mail: &mut dyn Mail,
    dir: &Path,
    agent: &str,
    condition: &str,
) -> Result<Vec<String>, RunError> {
    let dropped = pending(dir, agent).and_then(|faults| {
        faults
            .iter()
            .filter(|f| f.clears == condition)
            .try_for_each(|f| remove_pending(dir, agent, &f.fault_id))
    });
    let cleared = mail.fault_clear(agent, condition)?;
    dropped.map(|()| cleared)
}

/// What a loop boundary or an `agents up` start did for one agent's
/// faults: its pending files sent again (R35), then its retained receipts'
/// turn errors derived again (R42). The second also routes an active
/// occurrence to a changed `reports_to`: the store sends it to the new
/// recipient under the next id and supersedes the old copy (A24, A26).
#[derive(Debug, Default)]
pub struct Boundary {
    pub resent: Vec<Delivered>,
    pub rederived: Vec<Delivered>,
    /// Pending files or receipts that couldn't be read; the rest still ran.
    pub errors: Vec<RunError>,
}

/// Runs a loop boundary's fault work for `agent`, in one call: at every
/// supervisor loop boundary and when `agents up` starts.
pub fn boundary(mail: &mut dyn Mail, dir: &Path, agent: &state::AgentState) -> Boundary {
    let mut done = Boundary::default();
    match resend(mail, dir, &agent.spec.name) {
        Ok(resent) => done.resent = resent,
        Err(e) => done.errors.push(e),
    }
    match rederive(mail, dir, agent) {
        Ok(rederived) => done.rederived = rederived,
        Err(e) => done.errors.push(e),
    }
    done
}

/// The outcome a set-aside receipt is acked with (A26).
pub const RECEIPT_OUTCOME: &str = "receipt";

/// The sender of every receipt, a reserved actor.
const RECEIPT_SENDER: &str = "mdkg:receipts";

/// Sets `agent`'s receipts aside unseen, as a supervised turn does (A26,
/// R5): acked like a launcher note, with the outcome `receipt`, and never
/// shown. Returns the ids acked. A plain inbox read already hides them;
/// this only keeps them from waiting out their day unacked. A receipt in a
/// mailbox the agent no longer holds is left for its holder.
pub fn set_aside_receipts(
    mail: &mut dyn Mail,
    agent: &str,
    limit: usize,
) -> Result<Vec<String>, RunError> {
    let mut acked = Vec::new();
    for _ in 0..10 {
        let page = mail.receipts(agent, limit)?;
        let ids: Vec<String> = page
            .messages
            .iter()
            .filter(|m| m.kind == "receipt" && m.sender == RECEIPT_SENDER)
            .map(|m| m.message_id.clone())
            .collect();
        if ids.is_empty() {
            break;
        }
        let mut all = true;
        match mail.ack(agent, &ids, RECEIPT_OUTCOME) {
            Ok(()) => acked.extend(ids),
            // One the agent doesn't hold refuses the batch: one at a time.
            Err(e) if e.code == "msg.not_holder" => {
                for id in ids {
                    match mail.ack(agent, std::slice::from_ref(&id), RECEIPT_OUTCOME) {
                        Ok(()) => acked.push(id),
                        Err(e) if e.code == "msg.not_holder" => all = false,
                        Err(e) => return Err(e),
                    }
                }
            }
            Err(e) => return Err(e),
        }
        // A page left partly unacked would be read again: stop.
        if !all || !page.more {
            break;
        }
    }
    Ok(acked)
}

/// A harness found logged out (A26's table, R8).
pub const LOGIN_CODE: &str = "run.not_logged_in";

/// What clears a login fault: that harness's check reporting logged in.
pub fn clears_login(harness: crate::profile::Harness) -> String {
    format!("the {} login check reports logged in", harness.as_str())
}

/// What a login check did to the agent's login fault. Only the harness's
/// answer decides whether turns run: a store error in the fault's own
/// bookkeeping is carried here for the call site to report, never turned
/// into a failed turn (review of 7ac7525f, #2).
#[derive(Debug)]
pub enum LoginSeen {
    /// Logged in, or no check to make (OpenCode's own config decides): the
    /// occurrences this cleared, or why the clear failed (seen again at
    /// the next check).
    In(Result<Vec<String>, RunError>),
    /// Logged out: none of the agent's turns run until a check reports
    /// logged in (R8). The fault's delivery, or why its id couldn't be
    /// derived (the next check derives it again).
    Out(Result<Box<Delivered>, RunError>),
}

impl LoginSeen {
    /// Whether the agent's turns may run.
    pub fn runs(&self) -> bool {
        matches!(self, LoginSeen::In(_))
    }
}

/// Acts on a login check's answer for `agent`'s harness, when `agents up`
/// starts it and before each of its turns (A26, R8). The call site makes
/// the check (`auth::login`); this never changes it. Logged out sends one
/// `run.not_logged_in` fault per spell: its occurrence is the harness and
/// the number of this code's cleared occurrences, which moves only when a
/// spell ends, so a fault lost to a crash is derived again with the same id
/// and a resend stops at the store's copy. Logged in clears it, and the
/// clear's `fault.clear` record starts the next spell's id, even when its
/// recipient acked the fault by hand.
pub fn login(
    mail: &mut dyn Mail,
    dir: &Path,
    agent: &state::AgentState,
    login: &crate::auth::Login,
) -> LoginSeen {
    let name = &agent.spec.name;
    let harness = agent.spec.harness;
    let clears = clears_login(harness);
    if !matches!(login, crate::auth::Login::Out) {
        return LoginSeen::In(clear(mail, dir, name, &clears));
    }
    LoginSeen::Out(mail.fault_clears(name, LOGIN_CODE).map(|spells| {
        let occurrence = format!("{}\n{spells}", harness.as_str());
        let mut fault = Fault::new(
            name,
            LOGIN_CODE,
            &occurrence,
            vec![agent.spec.reports_to.clone()],
            &clears,
            format!("run `{}`", crate::auth::login_command(harness)),
        );
        fault.worktree = Worktree::of(&agent.spec.cwd);
        Box::new(deliver(mail, dir, &fault))
    }))
}

/// A supervisor that exited non-zero or gave up on its startup sync or
/// lease check (A26's table).
pub const SUPERVISOR_CODE: &str = "run.supervisor_exited";

/// What clears a supervisor's fault.
pub const CLEARS_SUPERVISOR: &str =
    "a supervisor for the agent starts and passes its startup sync and lease check";

/// Reports `agent`'s supervisor `supervisor_id` as exited: when it exits
/// non-zero or gives up, or when the next start finds its lock and
/// heartbeat dead. Its occurrence is the supervisor.
pub fn supervisor_exited(
    mail: &mut dyn Mail,
    dir: &Path,
    agent: &state::AgentState,
    supervisor_id: &str,
) -> Delivered {
    let name = &agent.spec.name;
    let mut fault = Fault::new(
        name,
        SUPERVISOR_CODE,
        supervisor_id,
        vec![agent.spec.reports_to.clone()],
        CLEARS_SUPERVISOR,
        format!("{BIN} agents up {name}"),
    );
    fault.supervisor_id = Some(supervisor_id.into());
    fault.worktree = Worktree::of(&agent.spec.cwd);
    deliver(mail, dir, &fault)
}

/// Clears `agent`'s supervisor fault once a new supervisor has passed its
/// startup sync and lease check, in that start's transaction.
pub fn supervisor_started(
    mail: &mut dyn Mail,
    dir: &Path,
    agent: &str,
) -> Result<Vec<String>, RunError> {
    clear(mail, dir, agent, CLEARS_SUPERVISOR)
}

/// A thread the loop guard paused (A26's table).
pub const LOOP_CODE: &str = "run.loop_paused";

/// What clears a loop pause's fault. The fault is the agent's, not a
/// thread's: A26's "the paused thread runs again" reads, for it, as no
/// thread of the agent staying paused (orchestrator, #11670).
pub const CLEARS_LOOP: &str = "no thread of the agent stays paused";

/// Reports that the loop guard holds at least one of `agent`'s threads
/// paused: one fault per agent per pause spell (orchestrator, #11670). The
/// call site reports it at every loop while the paused set isn't empty;
/// its occurrence is the number of this code's cleared occurrences, so the
/// spell keeps one id (the store's copy stands) and the next spell gets a
/// new one. It names no thread: `agents status` lists them.
pub fn loop_paused(
    mail: &mut dyn Mail,
    dir: &Path,
    agent: &state::AgentState,
) -> Result<Delivered, RunError> {
    let name = &agent.spec.name;
    let spells = mail.fault_clears(name, LOOP_CODE)?;
    let mut fault = Fault::new(
        name,
        LOOP_CODE,
        &spells.to_string(),
        vec![agent.spec.reports_to.clone()],
        CLEARS_LOOP,
        format!("{BIN} agents status {name}"),
    );
    fault.worktree = Worktree::of(&agent.spec.cwd);
    Ok(deliver(mail, dir, &fault))
}

/// Clears `agent`'s loop-pause fault: called at every loop boundary where
/// its paused set is empty, not only when it empties, since a clear that
/// finds nothing open writes nothing.
pub fn loop_resumed(mail: &mut dyn Mail, dir: &Path, agent: &str) -> Result<Vec<String>, RunError> {
    clear(mail, dir, agent, CLEARS_LOOP)
}

/// One agent's pending faults: how many, or why they can't be read.
pub type PendingCount = (String, Result<usize, RunError>);

/// Every agent's pending faults, by agent name, for `agents status` and
/// `doctor`; agents with none aren't listed.
pub fn pending_all(dir: &Path) -> Result<Vec<PendingCount>, RunError> {
    let entries = match std::fs::read_dir(dir.join(FOLDER)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other.map_err(|e| RunError::io("pending faults unavailable", e))?,
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .map(|n| n.replace("%3A", ":"))
        .filter(|n| state::check_name(n).is_ok())
        .collect();
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| {
            let count = pending(dir, &name).map(|p| p.len());
            (name, count)
        })
        .filter(|(_, count)| !matches!(count, Ok(0)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_ids_are_deterministic_uuids_of_what_they_report() {
        let id = fault_id("a-mail", "run.agent_failed", "turn-1");
        assert_eq!(id, fault_id("a-mail", "run.agent_failed", "turn-1"));
        let parsed = uuid::Uuid::parse_str(&id).unwrap();
        assert_eq!(parsed.get_version_num(), 8);
        assert_eq!(parsed.get_variant(), uuid::Variant::RFC4122);
        for other in [
            fault_id("a-mail", "run.agent_failed", "turn-2"),
            fault_id("a-mail", "run.no_reply", "turn-1"),
            fault_id("b-mail", "run.agent_failed", "turn-1"),
            // The separator keeps fields apart.
            fault_id("a-mail\nrun", "agent_failed", "turn-1"),
        ] {
            assert_ne!(id, other);
        }
        let longest = message_id(&id, u64::MAX);
        assert!(longest.len() <= 128, "{longest}");
        assert_eq!(message_id(&id, 0), format!("fault.{id}.r0"));
    }

    #[test]
    fn a_fault_never_quotes_the_errors_text() {
        let fault = Fault::new(
            "a-mail",
            "run.agent_failed",
            "t",
            vec!["orchestrator".into()],
            CLEARS_TURN,
            "x".into(),
        );
        assert_eq!(
            fault.message,
            mdkg_format::Code::RunAgentFailed.summary().to_string()
        );
        let parts = fault.parts(2, "2026-10-30T00:00:00.000Z");
        assert_eq!(parts[1]["data"]["count"], 2);
        assert_eq!(parts[1]["data"]["schema"], SCHEMA);
        assert!(
            parts[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("run.agent_failed fault for a-mail: ")
        );
        let unknown = Fault::new("a", "x.unknown", "t", vec![], CLEARS_TURN, "x".into());
        assert!(!unknown.message.contains("x.unknown"));
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mdkg-fault-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Sends succeed or fail by switch; the store keeps the first copy.
    #[derive(Default)]
    struct Faults {
        fail: bool,
        stored: Vec<String>,
        sends: usize,
        /// Every fault sent, as sent.
        sent: Vec<Fault>,
        /// Unacked receipts, and the ids the agent doesn't hold.
        receipts: Vec<crate::mail::Message>,
        not_held: Vec<String>,
        /// Every ack: its ids and outcome.
        acks: Vec<(Vec<String>, String)>,
        /// Every clear, by agent and condition; the cleared count.
        clears: Vec<(String, String)>,
        cleared: u64,
    }
    impl Mail for Faults {
        fn fault_send(&mut self, fault: &Fault) -> Result<FaultSent, RunError> {
            self.sends += 1;
            if self.fail {
                return Err(RunError::failure("run.mail_failed", "store unavailable"));
            }
            self.sent.push(fault.clone());
            let id = message_id(&fault.fault_id, 0);
            if self.stored.contains(&id) {
                return Ok(FaultSent::Stood(id));
            }
            self.stored.push(id.clone());
            Ok(FaultSent::Sent(id))
        }
        fn fault_clear(&mut self, agent: &str, condition: &str) -> Result<Vec<String>, RunError> {
            if self.fail {
                return Err(RunError::failure("run.mail_failed", "store busy"));
            }
            self.clears.push((agent.into(), condition.into()));
            self.cleared += 1;
            Ok(vec!["cleared".into()])
        }
        fn fault_clears(&mut self, _: &str, _: &str) -> Result<u64, RunError> {
            if self.fail {
                return Err(RunError::failure("run.mail_failed", "store busy"));
            }
            Ok(self.cleared)
        }
        fn receipts(&mut self, _: &str, limit: usize) -> Result<crate::Inbox, RunError> {
            let messages: Vec<_> = self.receipts.iter().take(limit).cloned().collect();
            Ok(crate::Inbox {
                cursor: messages.iter().map(|m| m.seq).max(),
                more: self.receipts.len() > limit,
                messages,
            })
        }
        fn inbox(&mut self, _: &str, _: usize) -> Result<crate::Inbox, RunError> {
            unreachable!()
        }
        fn wait(
            &mut self,
            _: &str,
            _: Option<u64>,
            _: u64,
        ) -> Result<Option<crate::Inbox>, RunError> {
            unreachable!()
        }
        fn import(&mut self, _: &crate::mail::Import<'_>) -> Result<String, RunError> {
            unreachable!()
        }
        fn ack(&mut self, _: &str, ids: &[String], outcome: &str) -> Result<(), RunError> {
            if ids.iter().any(|id| self.not_held.contains(id)) {
                return Err(RunError::usage("msg.not_holder", "not yours"));
            }
            self.receipts.retain(|m| !ids.contains(&m.message_id));
            self.acks.push((ids.to_vec(), outcome.into()));
            Ok(())
        }
        fn claim(&mut self, _: &str) -> Result<(), RunError> {
            unreachable!()
        }
        fn release(&mut self, _: &str) -> Result<(), RunError> {
            unreachable!()
        }
        fn holder(&self) -> String {
            "test".into()
        }
        fn release_as(&mut self, _: &str, _: &str) -> Result<(), RunError> {
            unreachable!()
        }
        fn holds(&mut self, _: &str, _: &str, _: Option<&str>) -> Result<bool, RunError> {
            unreachable!()
        }
        fn note(&mut self, _: &crate::mail::Note<'_>) -> Result<String, RunError> {
            unreachable!()
        }
    }

    fn sample(occurrence: &str) -> Fault {
        Fault::new(
            "lane",
            "run.agent_failed",
            occurrence,
            vec!["boss".into()],
            CLEARS_TURN,
            "mdkg agents status lane".into(),
        )
    }

    #[test]
    fn a_failed_send_keeps_its_private_pending_file_for_the_next_boundary() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = Scratch::new();
        let fault = sample("t1");
        let mut mail = Faults {
            fail: true,
            ..Faults::default()
        };
        let done = deliver(&mut mail, &scratch.0, &fault);
        assert!(done.send_error.is_some() && done.file_error.is_none());
        let path = pending_path(&scratch.0, "lane", &fault.fault_id);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(pending(&scratch.0, "lane").unwrap(), vec![fault.clone()]);
        // R35: the next boundary sends it, under the same id, and deletes it.
        mail.fail = false;
        let again = resend(&mut mail, &scratch.0, "lane").unwrap();
        assert_eq!(
            again[0].sent,
            Some(FaultSent::Sent(message_id(&fault.fault_id, 0)))
        );
        assert!(!path.exists());
        assert!(pending(&scratch.0, "lane").unwrap().is_empty());
        assert!(resend(&mut mail, &scratch.0, "lane").unwrap().is_empty());
    }

    #[test]
    fn a_file_left_by_a_crash_after_the_send_resends_as_a_no_op() {
        let scratch = Scratch::new();
        let fault = sample("t1");
        let mut mail = Faults::default();
        deliver(&mut mail, &scratch.0, &fault);
        // A crash between the send and the delete leaves the file.
        write_pending(&scratch.0, &fault).unwrap();
        let again = resend(&mut mail, &scratch.0, "lane").unwrap();
        assert_eq!(
            again[0].sent,
            Some(FaultSent::Stood(message_id(&fault.fault_id, 0)))
        );
        assert_eq!(mail.stored.len(), 1, "stored once");
        assert!(pending(&scratch.0, "lane").unwrap().is_empty());
    }

    #[test]
    fn an_unwritable_pending_file_still_sends() {
        let scratch = Scratch::new();
        // A file where the folder should be: the pending write fails.
        std::fs::write(scratch.0.join(FOLDER), b"obstruction").unwrap();
        let fault = sample("t1");
        let mut mail = Faults::default();
        let done = deliver(&mut mail, &scratch.0, &fault);
        assert!(done.file_error.is_some(), "{done:?}");
        assert_eq!(
            done.sent,
            Some(FaultSent::Sent(message_id(&fault.fault_id, 0))),
            "R42: the send is still tried"
        );
    }

    #[test]
    fn pending_files_that_dont_match_their_name_or_agent_are_refused() {
        let scratch = Scratch::new();
        let fault = sample("t1");
        write_pending(&scratch.0, &fault).unwrap();
        let path = pending_path(&scratch.0, "lane", &fault.fault_id);
        let other = pending_path(&scratch.0, "lane", &fault_id("x", "y", "z"));
        std::fs::rename(&path, &other).unwrap();
        assert!(pending(&scratch.0, "lane").is_err());
        assert!(
            write_pending(
                &scratch.0,
                &Fault {
                    agent: "Not A Name".into(),
                    ..fault.clone()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn a_receipts_ending_and_held_nodes_are_read_back() {
        let data = json!({"schema": "mdkg.turn.receipt.v1", "turn_id": "t",
            "result": {"status": "error", "code": "run.agent_failed", "message": "harness said x"},
            "activity": {"claims": [{"lease": "lease-1", "scope": ".mdkg/work/a.md", "node": "task-1"},
                {"lease": "lease-2", "scope": "src", "node": null},
                {"lease": "lease-3", "scope": ".mdkg/work/b.md", "node": "task-2"}],
                "leases_held": [{"id": "lease-1"}, {"id": "lease-2"}]}});
        let body = format!("Advisory\n{data}\ncompletion\n");
        let e = ended(&body).unwrap();
        assert_eq!(e.error.as_deref(), Some("run.agent_failed"));
        assert_eq!(e.nodes, vec!["task-1".to_string()]);
        assert!(ended("no json here").is_none());
    }

    fn agent(name: &str, reports_to: &str, harness: &str) -> state::AgentState {
        serde_json::from_value(json!({"spec": {
            "name": name, "harness": harness, "role": "worker",
            "cwd": "/nonexistent/mdkg-fault-test", "reports_to": reports_to,
        }}))
        .unwrap()
    }

    #[test]
    fn a_boundary_resends_pending_files_and_routes_a_receipts_fault_to_a_new_reports_to() {
        let scratch = Scratch::new();
        let mut mail = Faults {
            fail: true,
            ..Faults::default()
        };
        // A send that failed left its pending file.
        let left = sample("t0");
        deliver(&mut mail, &scratch.0, &left);
        mail.fail = false;
        // Retained receipts of two failed turns, since the agent's last good
        // one: only the later can be open, so only it is derived again.
        let receipts = scratch.0.join("receipts").join("lane");
        std::fs::create_dir_all(&receipts).unwrap();
        let mut turn = String::new();
        for _ in 0..2 {
            turn = uuid::Uuid::now_v7().to_string();
            let data = json!({"schema": "mdkg.turn.receipt.v1", "turn_id": turn,
                "result": {"status": "error", "code": "run.agent_failed"}});
            std::fs::write(receipts.join(format!("{turn}.txt")), format!("x\n{data}\n")).unwrap();
        }
        let done = boundary(&mut mail, &scratch.0, &agent("lane", "new-boss", "claude"));
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert_eq!(done.resent.len(), 1);
        assert_eq!(done.resent[0].fault_id, left.fault_id);
        assert_eq!(done.rederived.len(), 1);
        let rederived = mail.sent.last().unwrap();
        assert_eq!(
            rederived.fault_id,
            fault_id("lane", "run.agent_failed", &turn)
        );
        // The store routes it on from the old recipient's copy (A26).
        assert_eq!(rederived.to, vec!["new-boss".to_string()]);
        assert!(pending(&scratch.0, "lane").unwrap().is_empty());
    }

    fn message(id: &str, kind: &str, sender: &str) -> crate::mail::Message {
        crate::mail::Message {
            message_id: id.into(),
            seq: 1,
            context_id: None,
            kind: kind.into(),
            sender: sender.into(),
            via: None,
            room: None,
            sent_at: None,
            digest: String::new(),
            nodes: Vec::new(),
            supersedes: Vec::new(),
            parts: Vec::new(),
        }
    }

    #[test]
    fn receipts_are_set_aside_unseen_with_the_outcome_receipt() {
        let mut mail = Faults {
            receipts: vec![
                message("r1", "receipt", "mdkg:receipts"),
                message("r2", "receipt", "mdkg:receipts"),
                message("r3", "receipt", "mdkg:receipts"),
            ],
            ..Faults::default()
        };
        let acked = set_aside_receipts(&mut mail, "lane", 2).unwrap();
        assert_eq!(acked, vec!["r1", "r2", "r3"]);
        assert!(mail.acks.iter().all(|(_, o)| o == RECEIPT_OUTCOME));
        // One in a mailbox the agent doesn't hold is left for its holder.
        let mut mail = Faults {
            receipts: vec![
                message("r1", "receipt", "mdkg:receipts"),
                message("r2", "receipt", "mdkg:receipts"),
            ],
            not_held: vec!["r1".into()],
            ..Faults::default()
        };
        assert_eq!(
            set_aside_receipts(&mut mail, "lane", 10).unwrap(),
            vec!["r2"]
        );
        assert_eq!(mail.receipts.len(), 1);
        // Nothing else is acked as a receipt.
        let mut mail = Faults {
            receipts: vec![message("n1", "note", "boss")],
            ..Faults::default()
        };
        assert!(
            set_aside_receipts(&mut mail, "lane", 10)
                .unwrap()
                .is_empty()
        );
        assert!(mail.acks.is_empty());
    }

    #[test]
    fn a_login_fault_keeps_its_id_through_a_spell_and_a_new_spell_gets_a_new_one() {
        use crate::auth::Login;
        let scratch = Scratch::new();
        let lane = agent("lane", "boss", "codex");
        let mut mail = Faults::default();
        let LoginSeen::Out(Ok(first)) = login(&mut mail, &scratch.0, &lane, &Login::Out) else {
            panic!("logged out");
        };
        let id = first.fault_id.clone();
        assert_eq!(id, fault_id("lane", LOGIN_CODE, "codex\n0"));
        assert_eq!(mail.sent[0].to, vec!["boss".to_string()]);
        assert_eq!(mail.sent[0].clears, clears_login(lane.spec.harness));
        // A check while still logged out derives the same occurrence: the
        // store's copy stands.
        let LoginSeen::Out(Ok(again)) = login(&mut mail, &scratch.0, &lane, &Login::Out) else {
            panic!("logged out");
        };
        assert_eq!(again.fault_id, id);
        assert_eq!(again.sent, Some(FaultSent::Stood(message_id(&id, 0))));
        // Logged in clears it, by this harness's condition.
        let seen = login(
            &mut mail,
            &scratch.0,
            &lane,
            &Login::In {
                method: "ChatGPT".into(),
                subscription: true,
            },
        );
        assert!(seen.runs());
        assert!(matches!(seen, LoginSeen::In(Ok(ref c)) if c.len() == 1));
        assert_eq!(
            mail.clears,
            vec![("lane".to_string(), clears_login(lane.spec.harness))]
        );
        // The next spell is a new occurrence.
        let LoginSeen::Out(Ok(next)) = login(&mut mail, &scratch.0, &lane, &Login::Out) else {
            panic!("logged out");
        };
        assert_eq!(next.fault_id, fault_id("lane", LOGIN_CODE, "codex\n1"));
        // OpenCode's own config needs no check: nothing to report.
        let opencode = agent("oc", "boss", "opencode");
        assert!(login(&mut mail, &scratch.0, &opencode, &Login::Own).runs());
    }

    #[test]
    fn a_store_error_in_the_login_faults_bookkeeping_never_decides_whether_turns_run() {
        use crate::auth::Login;
        let scratch = Scratch::new();
        let lane = agent("lane", "boss", "claude");
        let mut mail = Faults {
            fail: true,
            ..Faults::default()
        };
        // Logged in with the store busy: the clear fails, the turns run.
        let seen = login(
            &mut mail,
            &scratch.0,
            &lane,
            &Login::In {
                method: "claude.ai".into(),
                subscription: true,
            },
        );
        assert!(seen.runs());
        assert!(matches!(seen, LoginSeen::In(Err(_))));
        // Logged out with the store busy: the id can't be derived, and the
        // turns still don't run.
        let seen = login(&mut mail, &scratch.0, &lane, &Login::Out);
        assert!(!seen.runs());
        assert!(matches!(seen, LoginSeen::Out(Err(_))));
    }

    #[test]
    fn supervisor_and_loop_faults_name_their_occurrence_and_clear_by_their_condition() {
        let scratch = Scratch::new();
        let lane = agent("lane", "boss", "claude");
        let mut mail = Faults::default();
        let exited = supervisor_exited(&mut mail, &scratch.0, &lane, "sup-1");
        assert_eq!(exited.fault_id, fault_id("lane", SUPERVISOR_CODE, "sup-1"));
        assert_eq!(mail.sent[0].supervisor_id.as_deref(), Some("sup-1"));
        assert_eq!(
            supervisor_started(&mut mail, &scratch.0, "lane")
                .unwrap()
                .len(),
            1
        );
        // One fault per pause spell, whichever threads it holds.
        let paused = loop_paused(&mut mail, &scratch.0, &lane).unwrap();
        let again = loop_paused(&mut mail, &scratch.0, &lane).unwrap();
        assert_eq!(
            paused.fault_id, again.fault_id,
            "the same spell, the same id"
        );
        assert_eq!(
            again.sent,
            Some(FaultSent::Stood(message_id(&paused.fault_id, 0)))
        );
        assert_eq!(mail.sent[1].suggested, format!("{BIN} agents status lane"));
        loop_resumed(&mut mail, &scratch.0, "lane").unwrap();
        let next = loop_paused(&mut mail, &scratch.0, &lane).unwrap();
        assert_ne!(next.fault_id, paused.fault_id, "a new spell, a new id");
        assert_eq!(
            mail.clears,
            vec![
                ("lane".to_string(), CLEARS_SUPERVISOR.to_string()),
                ("lane".to_string(), CLEARS_LOOP.to_string()),
            ]
        );
    }

    #[test]
    fn pending_faults_are_listed_per_agent() {
        let scratch = Scratch::new();
        assert!(pending_all(&scratch.0).unwrap().is_empty());
        let mut mail = Faults {
            fail: true,
            ..Faults::default()
        };
        deliver(&mut mail, &scratch.0, &sample("t1"));
        deliver(&mut mail, &scratch.0, &sample("t2"));
        let colon = Fault {
            agent: "codex:pr19".into(),
            ..sample("t1")
        };
        deliver(&mut mail, &scratch.0, &colon);
        let listed: Vec<(String, usize)> = pending_all(&scratch.0)
            .unwrap()
            .into_iter()
            .map(|(a, n)| (a, n.unwrap()))
            .collect();
        assert_eq!(
            listed,
            vec![("codex:pr19".to_string(), 1), ("lane".to_string(), 2)]
        );
        // Once sent, an agent with none isn't listed.
        mail.fail = false;
        resend(&mut mail, &scratch.0, "lane").unwrap();
        assert_eq!(pending_all(&scratch.0).unwrap().len(), 1);
    }

    #[test]
    fn a_clear_drops_a_pending_fault_of_its_condition_so_no_resend_reopens_it() {
        let scratch = Scratch::new();
        // Two sends that couldn't reach the store left their files: a
        // pause's and a failed turn's.
        let mut mail = Faults {
            fail: true,
            ..Faults::default()
        };
        let pause = Fault::new(
            "lane",
            LOOP_CODE,
            "0",
            vec!["boss".into()],
            CLEARS_LOOP,
            "x".into(),
        );
        let turn = sample("t1");
        deliver(&mut mail, &scratch.0, &pause);
        deliver(&mut mail, &scratch.0, &turn);
        assert_eq!(pending(&scratch.0, "lane").unwrap().len(), 2);
        // The pause ends: its file goes with the clear; the turn's stays.
        mail.fail = false;
        loop_resumed(&mut mail, &scratch.0, "lane").unwrap();
        assert_eq!(pending(&scratch.0, "lane").unwrap(), vec![turn.clone()]);
        // The next boundary sends only the turn's: nothing reopens the
        // ended pause.
        let sent = resend(&mut mail, &scratch.0, "lane").unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].fault_id, turn.fault_id);
        assert!(mail.sent.iter().all(|f| f.code != LOOP_CODE));
    }
}
