//! Launcher-owned, advisory turn receipts. Admission data stays in process;
//! immutable receipt bytes are retained separately from disposable turn logs.
//! This is not authentication against another process running as the same user.
//! No recovery path here reruns a harness or automatically resends a receipt.

use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use mdkg_coord_sqlite::ActivityCursor;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fault;
use crate::install::at::{self, Links, Tree};
use crate::mail::{Mail, Receipt};
use crate::state::{self, AgentState, Pinned};
use crate::{RunError, TurnOptions, TurnReport};

const MAX_LOCAL_BYTES: usize = 4 << 20;
const MAX_STATUS_BYTES: u64 = 64 << 10;
// Escaped JSON bytes, not Unicode scalar count. The wire copy is escaped
// again by ordinary mail transport; leave room for evidence and framing.
const MAX_MAILED_SUMMARY_BYTES: usize = 8 << 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryStatus {
    pub turn_id: String,
    /// Immutable local receipt, retained even when mail delivery is refused.
    pub path: Option<PathBuf>,
    pub message_id: Option<String>,
    /// Non-fatal receipt failure; never means the agent should run again.
    pub error: Option<String>,
}

pub(in crate::turn) struct Admission {
    id: String,
    agent: String,
    to: String,
    launcher: Option<String>,
    pin: Pinned,
    start: Option<String>,
    graph_root: Result<String, String>,
    baseline: Result<ActivityCursor, RunError>,
    number: u64,
    shown: Vec<String>,
    pub summary: Option<String>,
    pub reply_id: Option<String>,
    pub reply_status: &'static str,
    pub acknowledged: Vec<String>,
    pub not_held: Vec<String>,
}

impl Admission {
    pub(in crate::turn) fn capture(
        mail: &mut dyn Mail,
        agent: &AgentState,
        opts: &TurnOptions,
        id: String,
        start: Option<String>,
        number: u64,
        shown: Vec<String>,
    ) -> Self {
        let graph_root = match (&opts.graph_root, &opts.launcher_root) {
            (Some(root), Some(top)) => root
                .strip_prefix(top)
                .ok()
                .and_then(Path::to_str)
                .map(String::from)
                .ok_or_else(|| "graph root is outside the captured launcher worktree".into()),
            (None, _) => Ok(String::new()),
            _ => Err("graph root has no launcher worktree pin".into()),
        };
        Self {
            id,
            agent: agent.spec.name.clone(),
            to: agent.spec.reports_to.clone(),
            launcher: mail.writer(),
            pin: agent.spec.pinned.clone().expect("admitted pinned agent"),
            start,
            graph_root,
            baseline: mail.receipt_baseline(),
            number,
            shown,
            summary: None,
            reply_id: None,
            reply_status: "not produced",
            acknowledged: Vec::new(),
            not_held: Vec::new(),
        }
    }

    fn render(
        &self,
        mail: &mut dyn Mail,
        agent: &AgentState,
        result: &Result<TurnReport, RunError>,
        exits: &[Value],
    ) -> Result<(String, Value), RunError> {
        let span = super::harvest(&self.pin, self.start.as_deref());
        let activity = match &self.baseline {
            Ok(cursor) => mail.receipt_activity(cursor, &self.agent),
            Err(e) => Err(e.clone()),
        };
        let tasks = match (&span, &self.graph_root) {
            (Ok(span), Ok(root)) => mail.receipt_tasks(&self.pin, span, root),
            (Err(e), _) => Err(e.clone()),
            (_, Err(e)) => Err(RunError::failure(crate::error::CODE_MAIL_FAILED, e)),
        };
        let span_data = match &span {
            Ok(span) => json!({"branch":span.branch,"head":span.head,"start":span.start,
                "commits":span.commits.iter().map(|c| &c.id).collect::<Vec<_>>() }),
            Err(e) => json!({"unavailable":e.message,"start":self.start}),
        };
        let activity_data = match &activity {
            Ok(a) => {
                json!({"claims":a.claims.iter().map(|c| json!({"lease":c.lease,"scope":c.scope,"node":c.node})).collect::<Vec<_>>(), "leases_held":a.leases})
            }
            Err(e) => json!({"unavailable":e.message}),
        };
        let facts = activity
            .as_ref()
            .map(|a| a.completions.as_slice())
            .unwrap_or_default();
        let evidence = tasks.as_ref().cloned().unwrap_or_default();
        let completed = if activity.is_err() && tasks.is_err() {
            "Completion evidence unavailable: native activity and commit task evidence could not be collected.\n".into()
        } else {
            let section =
                super::tasks::render_completed(facts, &evidence.completed, &evidence.metadata)?;
            if activity.is_err() || tasks.is_err() {
                format!(
                    "Completion evidence is partial; unavailable sources are listed in the receipt data.\n{section}"
                )
            } else {
                section
            }
        };
        let result_data = match result {
            Ok(r) => json!({"status":"ok","outcome":r.outcome}),
            Err(e) => json!({"status":"error","code":e.code,"message":e.message}),
        };
        let data = json!({"schema":"mdkg.turn.receipt.v1","turn_id":self.id,
            "agent":self.agent,"launcher_actor":self.launcher,"reports_to":self.to,
            "turn":self.number,"harness":agent.spec.harness,"role":agent.spec.role,
            "session":agent.session,"shown":self.shown,"result":result_data,
            "process_exits":exits,"git":span_data,"activity":activity_data,
            "tasks_touched":tasks.as_ref().ok().map(|t| &t.touched),"task_evidence_error":tasks.as_ref().err().map(|e| &e.message),
            "completion_facts":activity.as_ref().ok().map(|a| a.completions.iter().map(|c| json!({"node":c.node,"revision":c.revision})).collect::<Vec<_>>()),
            "committed_done":tasks.as_ref().ok().map(|t| t.completed.iter().map(|c| json!({"node":c.node,"revision":c.revision})).collect::<Vec<_>>()),
            "reply_status":self.reply_status,"reply_id":self.reply_id,"acknowledged":self.acknowledged,
            "ack_not_held":self.not_held,"final_summary":self.summary});
        let mut body = String::from(
            "Advisory launcher turn receipt; fields report observations, not authorship or approval.\n",
        );
        if let Ok(span) = &span {
            body.push_str(&format!(
                "commits in the observed turn span ({}..{})\n",
                span.start.as_deref().unwrap_or("unborn"),
                span.head.as_deref().unwrap_or("unborn")
            ));
        }
        body.push_str(&ascii_json(&data)?);
        body.push('\n');
        body.push_str(&completed);
        if body.len() > MAX_LOCAL_BYTES {
            return Err(RunError::failure(
                crate::error::CODE_MAIL_FAILED,
                "receipt rendering exceeded the 4 MiB local limit",
            ));
        }
        Ok((body, data))
    }

    pub(in crate::turn) fn finish(
        self,
        mail: &mut dyn Mail,
        dir: &Path,
        agent: &AgentState,
        result: &Result<TurnReport, RunError>,
        exits: &[Value],
    ) -> DeliveryStatus {
        let mut status = DeliveryStatus {
            turn_id: self.id.clone(),
            path: None,
            message_id: None,
            error: None,
        };
        let mut data = None;
        let sent = (|| {
            status.error = Some("receipt retention pending; delivery not attempted".into());
            save_status(dir, &self.agent, &status)?;
            let (body, rendered) = self.render(mail, agent, result, exits)?;
            data = Some(rendered);
            status.path = Some(retain(dir, &self.agent, &self.id, body.as_bytes())?);
            status.error = Some(
                "receipt delivery pending or interrupted; outcome unknown; no automatic resend"
                    .into(),
            );
            save_status(dir, &self.agent, &status)?;
            state::check_reports_to(&self.agent, &self.to)?;
            let mailed = mail_copy(&body, status.path.as_deref().expect("retained receipt"))?;
            status.message_id = Some(mail.receipt_send(&Receipt {
                id: &self.id,
                agent: &self.agent,
                to: &self.to,
                body: &mailed,
            })?);
            Ok::<_, RunError>(())
        })();
        status.error = sent.err().map(|e| e.message);
        // After the receipt, its source, is retained (A26): a turn that
        // ended in an error is a fault to `reports_to` (R1), and one that
        // didn't clears the agent's turn faults (R3). Neither changes the
        // turn's own result.
        let quiet = status.error.is_none();
        if let Some(problem) = self.fault(mail, dir, agent, result, data.as_ref(), quiet) {
            status.error = Some(match status.error {
                Some(error) => format!("{error}; {problem}"),
                None => problem,
            });
        }
        if let Err(e) = save_status(dir, &self.agent, &status) {
            status.error = Some(match status.error {
                Some(error) => format!("{error}; {}", e.message),
                None => e.message,
            });
        }
        status
    }
}

impl Admission {
    /// The turn's fault, or its clear; a problem worth reporting, if any.
    fn fault(
        &self,
        mail: &mut dyn Mail,
        dir: &Path,
        agent: &AgentState,
        result: &Result<TurnReport, RunError>,
        data: Option<&Value>,
        quiet: bool,
    ) -> Option<String> {
        match result {
            Err(e) => {
                let mut fault = fault::Fault::turn_error(agent, &self.to, &self.id, &e.code);
                fault.nodes = data.map(fault::receipt_nodes).unwrap_or_default();
                let done = fault::deliver(mail, dir, &fault);
                match (&done.send_error, &done.file_error) {
                    (None, _) => None,
                    (Some(send), None) => Some(format!(
                        "fault {} not sent yet; its pending file is sent again at the next loop boundary: {}",
                        fault.fault_id, send.message
                    )),
                    (Some(send), Some(file)) => Some(format!(
                        "fault {} not sent, and its pending file not written; the next start derives it again from this receipt: {}; {}",
                        fault.fault_id, send.message, file.message
                    )),
                }
            }
            // A failed clear is seen again at the next turn without an
            // error; it is reported only when nothing else was.
            Ok(report) => {
                let mut cleared = fault::clear(mail, dir, &self.agent, fault::CLEARS_TURN);
                if report.outcome == crate::Outcome::Replied {
                    cleared =
                        cleared.and(fault::clear(mail, dir, &self.agent, fault::CLEARS_REPLY));
                }
                cleared
                    .err()
                    .filter(|_| quiet)
                    .map(|e| format!("turn faults not cleared: {}", e.message))
            }
        }
    }
}

/// Only the mailed summary is shortened. The full deterministic receipt is
/// already durable, and all other evidence and its framing remain unchanged.
fn mail_copy(body: &str, path: &Path) -> Result<String, RunError> {
    let invalid =
        || RunError::failure(crate::error::CODE_MAIL_FAILED, "invalid local receipt data");
    let start = body.find("\n{").map(|i| i + 1).ok_or_else(invalid)?;
    let end = start + body[start..].find('\n').ok_or_else(invalid)?;
    let mut data: Value = serde_json::from_str(&body[start..end]).map_err(|_| invalid())?;
    let Some(summary) = data.get("final_summary").and_then(Value::as_str) else {
        return Ok(body.into());
    };
    let mut used = 0;
    let mut split = None;
    for (offset, c) in summary.char_indices() {
        let bytes = ascii_json(&json!(c.to_string()))?.len() - 2;
        if used + bytes > MAX_MAILED_SUMMARY_BYTES {
            split = Some(offset);
            break;
        }
        used += bytes;
    }
    let Some(split) = split else {
        return Ok(body.into());
    };
    let original_bytes = summary.len();
    data["final_summary"] = json!(&summary[..split]);
    data["final_summary_truncated"] = json!(true);
    data["final_summary_original_bytes"] = json!(original_bytes);
    data["local_receipt"] = json!(path);
    Ok(format!(
        "{}{}{}",
        &body[..start],
        ascii_json(&data)?,
        &body[end..]
    ))
}

fn ascii_json(value: &Value) -> Result<String, RunError> {
    use std::fmt::Write as _;
    let serialized = serde_json::to_string(value)
        .map_err(|e| RunError::io("can't serialize receipt", e.into()))?;
    let mut out = String::new();
    for c in serialized.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                write!(out, "\\u{unit:04x}").expect("String write");
            }
        }
        if out.len() > MAX_LOCAL_BYTES {
            return Err(RunError::failure(
                crate::error::CODE_MAIL_FAILED,
                "receipt rendering exceeded the 4 MiB local limit",
            ));
        }
    }
    Ok(out)
}

fn folders(dir: &Path, create: bool) -> std::io::Result<(at::Folder, Tree)> {
    // dir was selected from the checked common directory, not agent text.
    let run = Tree::open(dir)?;
    let root = run.folder(Path::new(""), false, Links::Never)?;
    let tree = run.subtree(Path::new("receipts"), create)?;
    if create {
        rustix::fs::fsync(&root.fd)?;
    }
    Ok((tree.folder(Path::new(""), false, Links::Never)?, tree))
}

fn publish(folder: &at::Folder, name: &str, bytes: &[u8], immutable: bool) -> std::io::Result<()> {
    let temporary = format!(".{}.tmp", uuid::Uuid::now_v7());
    let result = (|| {
        let mut file = at::create_file(&folder.fd, OsStr::new(&temporary))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if immutable {
            at::rename_noreplace(
                &folder.fd,
                OsStr::new(&temporary),
                &folder.fd,
                OsStr::new(name),
            )?;
        } else {
            at::rename(
                &folder.fd,
                OsStr::new(&temporary),
                &folder.fd,
                OsStr::new(name),
            )?;
        }
        rustix::fs::fsync(&folder.fd)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = at::unlink(&folder.fd, OsStr::new(&temporary));
    }
    result
}

fn retain(dir: &Path, agent: &str, id: &str, bytes: &[u8]) -> Result<PathBuf, RunError> {
    state::check_name(agent)?;
    uuid::Uuid::parse_str(id)
        .map_err(|_| RunError::usage(crate::error::CODE_MAIL_FAILED, "invalid receipt turn id"))?;
    let save = || -> std::io::Result<PathBuf> {
        let (receipts, tree) = folders(dir, true)?;
        let rel = state::file_name(agent);
        let folder = tree.folder(Path::new(&rel), true, Links::Never)?;
        rustix::fs::fsync(&receipts.fd)?;
        let name = format!("{id}.txt");
        publish(&folder, &name, bytes, true)?;
        Ok(dir.join("receipts").join(rel).join(name))
    };
    save().map_err(|e| RunError::io("can't retain local receipt", e))
}

fn save_status(dir: &Path, agent: &str, status: &DeliveryStatus) -> Result<(), RunError> {
    let save = || -> std::io::Result<()> {
        let (folder, _) = folders(dir, true)?;
        let bytes = serde_json::to_vec(status)?;
        if bytes.len() as u64 > MAX_STATUS_BYTES {
            return Err(std::io::Error::other("receipt status exceeds its limit"));
        }
        publish(
            &folder,
            &format!("{}.json", state::file_name(agent)),
            &bytes,
            false,
        )
    };
    save().map_err(|e| RunError::io("can't save receipt delivery status", e))
}

/// Read-only status, bounded and never following a receipt-directory/file link.
/// Missing means no receipt yet; corrupt/unreadable means unavailable, not success.
pub fn latest(dir: &Path, agent: &str) -> Result<Option<DeliveryStatus>, RunError> {
    state::check_name(agent)?;
    let read = || -> std::io::Result<DeliveryStatus> {
        let (folder, _) = folders(dir, false)?;
        let file = at::open_file(
            &folder.fd,
            OsStr::new(&format!("{}.json", state::file_name(agent))),
        )?;
        let mut bytes = Vec::new();
        file.take(MAX_STATUS_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_STATUS_BYTES {
            return Err(std::io::Error::other("receipt status exceeds its limit"));
        }
        let value: DeliveryStatus = serde_json::from_slice(&bytes)?;
        uuid::Uuid::parse_str(&value.turn_id).map_err(std::io::Error::other)?;
        let expected = dir
            .join("receipts")
            .join(state::file_name(agent))
            .join(format!("{}.txt", value.turn_id));
        if value.path.as_ref().is_some_and(|p| *p != expected) {
            return Err(std::io::Error::other(
                "receipt status path does not match its turn",
            ));
        }
        Ok(value)
    };
    match read() {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(RunError::io("receipt delivery status unavailable", e)),
    }
}

pub(in crate::turn) fn attach(result: &mut Result<TurnReport, RunError>, status: DeliveryStatus) {
    match result {
        Ok(report) => report.receipt = Some(status),
        Err(e) => *e = e.clone().and_hint(diagnostic(&status)),
    }
}

pub(in crate::turn) fn diagnostic(status: &DeliveryStatus) -> String {
    let path = status
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "none".into());
    match &status.error {
        Some(error) => format!("receipt: {error}; local receipt: {path}"),
        None => format!(
            "receipt delivered as {}; local receipt: {path}",
            status.message_id.as_deref().unwrap_or("unknown"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("mdkg-receipt-storage-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn mailed_summary_preview_is_bounded_deterministic_and_keeps_other_evidence() {
        let summary = "日本語🙂\r\n\"\\".repeat(5000);
        let data = json!({"final_summary":summary,"other":"unchanged"});
        let body = format!(
            "Advisory\n{}\ncompletion frame unchanged\n",
            ascii_json(&data).unwrap()
        );
        let path = Path::new("/tmp/full receipt.txt");
        let mailed = mail_copy(&body, path).unwrap();
        assert_eq!(mailed, mail_copy(&body, path).unwrap());
        assert!(mailed.ends_with("\ncompletion frame unchanged\n"));
        let copied: Value = serde_json::from_str(mailed.lines().nth(1).unwrap()).unwrap();
        let preview = copied["final_summary"].as_str().unwrap();
        assert!(summary.starts_with(preview));
        assert!(ascii_json(&json!(preview)).unwrap().len() <= MAX_MAILED_SUMMARY_BYTES + 2);
        assert_eq!(copied["other"], data["other"]);
        assert_eq!(copied["local_receipt"], path.to_str().unwrap());
        assert_eq!(copied["final_summary_original_bytes"], summary.len());
        assert_eq!(copied["final_summary_truncated"], true);
        assert!(
            serde_json::to_vec(&json!([{"kind":"text","text":mailed}]))
                .unwrap()
                .len()
                < 64 * 1024
        );
        let short = "Advisory\n{\"final_summary\":\"short\"}\nframing\n";
        assert_eq!(mail_copy(short, path).unwrap(), short);
    }

    #[test]
    fn receipt_diagnostic_prints_plain_path_or_none() {
        let mut status = DeliveryStatus {
            turn_id: "id".into(),
            path: Some(PathBuf::from("/tmp/local receipt")),
            message_id: Some("message".into()),
            error: None,
        };
        assert_eq!(
            diagnostic(&status),
            "receipt delivered as message; local receipt: /tmp/local receipt"
        );
        status.path = None;
        status.error = Some("refused".into());
        assert_eq!(diagnostic(&status), "receipt: refused; local receipt: none");
    }

    #[test]
    fn receipts_are_private_immutable_and_status_reads_do_not_write() {
        let scratch = Scratch::new();
        let id = uuid::Uuid::now_v7().to_string();
        let path = retain(&scratch.0, "probe", &id, b"deterministic\n").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(retain(&scratch.0, "probe", &id, b"replacement").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"deterministic\n");
        let status = DeliveryStatus {
            turn_id: id,
            path: Some(path),
            message_id: None,
            error: Some("send refused".into()),
        };
        save_status(&scratch.0, "probe", &status).unwrap();
        let p = scratch.0.join("receipts/probe.json");
        let before = std::fs::read(&p).unwrap();
        assert_eq!(latest(&scratch.0, "probe").unwrap(), Some(status));
        assert_eq!(std::fs::read(p).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(scratch.0.join("receipts/probe"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn missing_status_is_read_only_and_obstructed_paths_fail_closed() {
        let scratch = Scratch::new();
        assert!(latest(&scratch.0, "probe").unwrap().is_none());
        assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 0);
        let other = Scratch::new();
        symlink(&other.0, scratch.0.join("receipts")).unwrap();
        assert!(
            retain(
                &scratch.0,
                "probe",
                &uuid::Uuid::now_v7().to_string(),
                b"bytes"
            )
            .is_err()
        );
        assert!(latest(&scratch.0, "probe").is_err());
        assert_eq!(std::fs::read_dir(&other.0).unwrap().count(), 0);
    }

    #[test]
    fn status_size_paths_and_links_are_checked() {
        let scratch = Scratch::new();
        std::fs::create_dir(scratch.0.join("receipts")).unwrap();
        let path = scratch.0.join("receipts/probe.json");
        std::fs::write(&path, vec![b'x'; MAX_STATUS_BYTES as usize + 1]).unwrap();
        assert!(
            latest(&scratch.0, "probe")
                .unwrap_err()
                .message
                .contains("limit")
        );
        let status = DeliveryStatus {
            turn_id: uuid::Uuid::now_v7().to_string(),
            path: Some(PathBuf::from("/elsewhere")),
            message_id: None,
            error: None,
        };
        std::fs::write(&path, serde_json::to_vec(&status).unwrap()).unwrap();
        assert!(
            latest(&scratch.0, "probe")
                .unwrap_err()
                .message
                .contains("does not match")
        );
        std::fs::remove_file(&path).unwrap();
        symlink("missing", path).unwrap();
        assert!(latest(&scratch.0, "probe").is_err());
    }

    #[test]
    fn receipt_json_is_deterministic_and_escapes_invisible_text() {
        let value = json!({"summary":"a\nforged\u{202e}\u{200b}\u{1b}💡"});
        let text = ascii_json(&value).unwrap();
        assert!(text.is_ascii());
        assert!(!text.contains('\n'));
        assert!(text.contains("\\u202e"));
        assert!(text.contains("\\u200b"));
        assert_eq!(text, ascii_json(&value).unwrap());
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), value);
    }
}
