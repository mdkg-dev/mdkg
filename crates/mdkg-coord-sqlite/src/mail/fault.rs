//! Faults in the mail store (a-flow A26): a fault's copies, its direct
//! dedupe, and its clearing.
//!
//! A fault is a message from [`FAULT_SENDER`] of kind `fault`, whose data
//! part (schema [`FAULT_SCHEMA`]) names its `agent`, `code`, `fault_id` and
//! the condition that `clears` it. Its copies are `fault.<fault_id>.r<k>`,
//! `k` counting the copies the store already holds, so a route A → B → A
//! gets three ids.
//!
//! Until a-flow's P2 moves it onto the `fault:<agent>:<code>` slot, the
//! dedupe is a direct lookup: in the send's transaction, the open faults
//! with the same agent and code are acked, at every mailbox, with `via:
//! "supersede"`.

use rusqlite::{Transaction, params};
use serde_json::Value;

use super::{
    FAULT_ID_PREFIX, MailError, Message, Outgoing, Retention, To, body_digest, message_by_id, ms,
    purge_due, purge_if_done, request_digest, send_in, sql_ms,
};
use crate::Store;

/// The only sender of faults (A26).
pub const FAULT_SENDER: &str = "mdkg:supervisor";

/// A fault's data part.
pub const FAULT_SCHEMA: &str = "mdkg.run.fault.v1";

/// The record a clear appends, once per occurrence.
pub const CLEAR_RECORD: &str = "fault.clear";

/// The actor of every system ack.
const MDKG: &str = "mdkg";

/// One fault to send: what the store needs to number, route and dedupe it.
#[derive(Debug, Clone)]
pub struct FaultOut<'a> {
    /// The occurrence's id, derived from what it reports.
    pub fault_id: &'a str,
    pub agent: &'a str,
    pub code: &'a str,
    pub to: &'a [String],
    pub human_hold_days: u32,
    pub now_ms: u64,
}

/// The count and first time a new copy carries (A26, R2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultTally {
    pub count: u64,
    /// RFC 3339 UTC.
    pub first_at: String,
}

/// What became of a fault send.
#[derive(Debug, Clone, PartialEq)]
pub enum FaultSent {
    /// Stored as this new copy; `superseded` names the open faults it acked.
    Sent {
        message: Message,
        superseded: Vec<String>,
    },
    /// The store's copy stands: the occurrence's latest copy already reached
    /// every recipient, or a later occurrence of the same agent and code
    /// replaced it. Nothing was stored, whatever the new body held.
    Stood(Message),
    /// The occurrence's condition already ended: it is never sent again.
    Cleared,
}

/// One occurrence a clear ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultCleared {
    pub code: String,
    pub fault_id: String,
    /// Its copies' deliveries this clear acked: (message id, mailbox).
    pub acked: Vec<(String, String)>,
}

/// A stored fault's identity, from its data part.
#[derive(Debug, Clone)]
struct Stored {
    message: Message,
    fault_id: String,
    agent: String,
    code: String,
    clears: Option<String>,
    count: u64,
    first_at: Option<String>,
}

fn data_part(parts: &Value) -> Option<&serde_json::Map<String, Value>> {
    parts.as_array()?.iter().find_map(|p| {
        let data = p.get("data")?.as_object()?;
        (data.get("schema")?.as_str()? == FAULT_SCHEMA).then_some(data)
    })
}

/// The fault a message is, or `None` for one whose body is gone or holds
/// no fault data part.
fn stored(message: Message) -> Option<Stored> {
    let data = data_part(message.parts.as_ref()?)?;
    let text = |k: &str| data.get(k).and_then(Value::as_str).map(String::from);
    Some(Stored {
        fault_id: text("fault_id")?,
        agent: text("agent")?,
        code: text("code")?,
        clears: text("clears"),
        count: data.get("count").and_then(Value::as_u64).unwrap_or(1),
        first_at: text("first_at"),
        message,
    })
}

/// Every fault message from mdkg, oldest first; with `open`, only those
/// with an unacked delivery.
fn faults(tx: &Transaction<'_>, open: bool) -> Result<Vec<Stored>, MailError> {
    let sql = if open {
        "SELECT DISTINCT m.message_id, m.seq FROM msg_messages m
         JOIN msg_deliveries d ON d.seq = m.seq
         WHERE m.sender = ?1 AND m.kind = 'fault' AND d.state = 'delivered' ORDER BY m.seq"
    } else {
        "SELECT message_id, seq FROM msg_messages
         WHERE sender = ?1 AND kind = 'fault' ORDER BY seq"
    };
    let ids: Vec<String> = tx
        .prepare(sql)?
        .query_map(params![FAULT_SENDER], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for id in ids {
        if let Some(m) = message_by_id(tx, &id)?
            && let Some(s) = stored(m)
        {
            out.push(s);
        }
    }
    Ok(out)
}

/// The copies of one occurrence, by `k`. Any other row under its ids is
/// store damage, never taken as the alert (A26).
fn copies(tx: &Transaction<'_>, fault_id: &str) -> Result<Vec<(u64, Message)>, MailError> {
    let prefix = format!("{FAULT_ID_PREFIX}{fault_id}.r");
    // Every id with the prefix sorts in [prefix, prefix with its last
    // byte one higher), so the message id's index finds them.
    let end = format!("{FAULT_ID_PREFIX}{fault_id}.s");
    let ids: Vec<String> = tx
        .prepare("SELECT message_id FROM msg_messages WHERE message_id >= ?1 AND message_id < ?2")?
        .query_map(params![prefix, end], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for id in ids {
        let message =
            message_by_id(tx, &id)?.ok_or_else(|| MailError::UnknownMessage(id.clone()))?;
        let suffix = &id[prefix.len()..];
        let k = suffix
            .parse::<u64>()
            .ok()
            .filter(|k| suffix == k.to_string());
        match k {
            Some(k) if message.sender == FAULT_SENDER && message.kind == "fault" => {
                out.push((k, message));
            }
            _ => return Err(MailError::Integrity(id)),
        }
    }
    out.sort_by_key(|(k, _)| *k);
    Ok(out)
}

/// Whether an occurrence's condition was seen to end: a copy acked by a
/// clear, or a `fault.clear` record naming it (a clear of copies already
/// acked by hand acks nothing, but records the end all the same).
fn cleared(tx: &Transaction<'_>, fault_id: &str, copies: &[Message]) -> Result<bool, MailError> {
    if copies.iter().any(|m| {
        m.deliveries
            .iter()
            .any(|d| d.acked_by.as_deref() == Some(MDKG) && d.acked_via.as_deref() == Some("clear"))
    }) {
        return Ok(true);
    }
    // Newest first: a clear comes after the occurrence's send, so the scan
    // stops early when one exists. The text match only narrows the scan;
    // the record's own kind and body field decide, so a matching string
    // embedded deeper in another record never counts.
    Ok(tx
        .prepare(
            "SELECT 1 FROM log WHERE record LIKE ?1 AND record LIKE ?2
               AND json_extract(record, '$.kind') = ?3
               AND json_extract(record, '$.body.fault_id') = ?4
             ORDER BY seq DESC LIMIT 1",
        )?
        .exists(params![
            format!("%\"kind\":\"{CLEAR_RECORD}\"%"),
            format!("%\"fault_id\":\"{fault_id}\"%"),
            CLEAR_RECORD,
            fault_id
        ])?)
}

/// Acks every unacked delivery of `messages`, at every mailbox, as mdkg,
/// with no listing needed: a system ack (A24, A26).
fn system_ack(
    tx: &Transaction<'_>,
    messages: &[&Message],
    via: &str,
    outcome: &str,
    now_ms: u64,
) -> Result<Vec<(String, String)>, MailError> {
    let mut newly: Vec<(String, String)> = Vec::new();
    let mut purged = Vec::new();
    for m in messages {
        for d in &m.deliveries {
            let changed = tx.execute(
                "UPDATE msg_deliveries SET state = 'acked', acked_by = ?3, acked_ms = ?4,
                     outcome = ?5, acked_via = ?6
                 WHERE seq = ?1 AND mailbox = ?2 AND state != 'acked'",
                params![sql_ms(m.seq), d.mailbox, MDKG, sql_ms(now_ms), outcome, via],
            )?;
            if changed > 0 {
                newly.push((m.message_id.clone(), d.mailbox.clone()));
            }
        }
        if purge_if_done(tx, m.seq, now_ms, 0)? {
            purged.push(m.message_id.clone());
        }
    }
    if !newly.is_empty() || !purged.is_empty() {
        let mut subject: Vec<String> = Vec::new();
        for id in newly.iter().map(|(id, _)| id).chain(&purged) {
            let s = format!("msg:{id}");
            if !subject.contains(&s) {
                subject.push(s);
            }
        }
        crate::log::append_mail(
            tx,
            MDKG,
            "msg.ack",
            subject,
            serde_json::json!({
                "acked": newly.iter().map(|(id, mailbox)| {
                    serde_json::json!({"message_id": id, "mailbox": mailbox})
                }).collect::<Vec<_>>(),
                "by": MDKG,
                "via": via,
                "outcome_digest": crate::log::text_digest(outcome),
                "purged": purged,
            }),
        )?;
    }
    Ok(newly)
}

fn send_fault_in(
    tx: &Transaction<'_>,
    f: &FaultOut<'_>,
    build: impl FnOnce(&FaultTally) -> Value,
) -> Result<FaultSent, MailError> {
    let held = copies(tx, f.fault_id)?;
    let messages: Vec<Message> = held.iter().map(|(_, m)| m.clone()).collect();
    if !held.is_empty() && cleared(tx, f.fault_id, &messages)? {
        return Ok(FaultSent::Cleared);
    }
    let open: Vec<Stored> = faults(tx, true)?
        .into_iter()
        .filter(|s| s.agent == f.agent && s.code == f.code)
        .collect();
    let tally = if let Some((_, latest)) = held.last() {
        // A resend stops at the store's copy, whatever its body: the latest
        // copy already went to every recipient.
        let reached =
            f.to.iter()
                .all(|to| latest.deliveries.iter().any(|d| &d.mailbox == to));
        // Only the active occurrence follows a changed recipient: one a
        // later fault of the same agent and code replaced stays where it is.
        let replaced = faults(tx, false)?.into_iter().any(|s| {
            s.agent == f.agent
                && s.code == f.code
                && s.fault_id != f.fault_id
                && s.message.seq > latest.seq
        });
        if reached || replaced {
            return Ok(FaultSent::Stood(latest.clone()));
        }
        // The same occurrence, routed again: its count stays.
        let same = stored(latest.clone());
        FaultTally {
            count: same.as_ref().map_or(1, |s| s.count),
            first_at: same
                .and_then(|s| s.first_at)
                .unwrap_or_else(|| mdkg_log::record::format_ts(latest.sent_ms)),
        }
    } else {
        match open.last() {
            Some(prior) => FaultTally {
                count: prior.count + 1,
                first_at: prior
                    .first_at
                    .clone()
                    .unwrap_or_else(|| mdkg_log::record::format_ts(prior.message.sent_ms)),
            },
            None => FaultTally {
                count: 1,
                first_at: mdkg_log::record::format_ts(f.now_ms),
            },
        }
    };
    let out = Outgoing {
        message_id: format!("{FAULT_ID_PREFIX}{}.r{}", f.fault_id, held.len()),
        context_id: None,
        kind: "fault".into(),
        sender: FAULT_SENDER.into(),
        via: None,
        sender_mailboxes: Vec::new(),
        to: To::Mailboxes(f.to.to_vec()),
        reference_ids: Vec::new(),
        parts: build(&tally),
        // Kept: a clear needs the agent, code and condition of a copy
        // acked by hand long ago.
        retention: Retention::Durable,
        human_hold_days: f.human_hold_days,
        now_ms: f.now_ms,
    };
    let (text, digest, size) = body_digest(&out.parts)?;
    let request = request_digest(&out, &digest)?;
    // No flow: a fault binds no node, and its dedupe is the direct lookup
    // above until P2's `fault:<agent>:<code>` slot. Under any policy a
    // fault never expires (A25).
    let (message, _, _) = send_in(
        tx,
        &out,
        None,
        &request,
        &text,
        &digest,
        size,
        &crate::ledger::SendFlow::default(),
        &crate::ledger::ExpiryPolicy::default(),
        0,
    )?;
    // One fault open per agent and code, at every mailbox it reached, the
    // new copy's own occurrence included (A24's fault-slot exception).
    let replaced: Vec<&Message> = open.iter().map(|s| &s.message).collect();
    let outcome = format!("superseded by {}", message.message_id);
    system_ack(tx, &replaced, "supersede", &outcome, f.now_ms)?;
    let message = message_by_id(tx, &message.message_id)?
        .ok_or_else(|| MailError::UnknownMessage(out.message_id.clone()))?;
    Ok(FaultSent::Sent {
        message,
        superseded: open.into_iter().map(|s| s.message.message_id).collect(),
    })
}

fn clear_in(
    tx: &Transaction<'_>,
    agent: &str,
    condition: &str,
    now_ms: u64,
) -> Result<Vec<FaultCleared>, MailError> {
    let all = faults(tx, false)?;
    let mut codes: Vec<&str> = all
        .iter()
        .filter(|s| s.agent == agent && s.clears.as_deref() == Some(condition))
        .map(|s| s.code.as_str())
        .collect();
    codes.sort_unstable();
    codes.dedup();
    let mut out = Vec::new();
    for code in codes {
        // The latest occurrence is the one whose condition is reported:
        // earlier ones were superseded by it, or cleared before it.
        let Some(latest) = all
            .iter()
            .rev()
            .find(|s| s.agent == agent && s.code == code)
        else {
            continue;
        };
        if latest.clears.as_deref() != Some(condition) {
            continue;
        }
        let fault_id = latest.fault_id.clone();
        let held: Vec<Message> = copies(tx, &fault_id)?.into_iter().map(|(_, m)| m).collect();
        if cleared(tx, &fault_id, &held)? {
            continue;
        }
        let refs: Vec<&Message> = held.iter().collect();
        let acked = system_ack(tx, &refs, "clear", &format!("cleared: {condition}"), now_ms)?;
        // Recorded whether or not anything was left to ack, so the end of
        // the condition is durable apart from acknowledgement (A26).
        crate::log::append_mail(
            tx,
            MDKG,
            CLEAR_RECORD,
            held.iter()
                .map(|m| format!("msg:{}", m.message_id))
                .collect(),
            serde_json::json!({"agent": agent, "code": code, "fault_id": fault_id}),
        )?;
        out.push(FaultCleared {
            code: code.to_string(),
            fault_id,
            acked,
        });
    }
    Ok(out)
}

impl Store {
    /// Sends one fault occurrence in one mail transaction (A26): numbers its
    /// copy, supersedes the open faults of the same agent and code at every
    /// mailbox, and carries their count on. `build` makes the body from the
    /// tally. A resend of an occurrence already stored to every recipient
    /// stores nothing ([`FaultSent::Stood`]), and a cleared one is never
    /// sent again.
    pub fn send_fault(
        &mut self,
        f: &FaultOut<'_>,
        build: impl FnOnce(&FaultTally) -> Value,
    ) -> Result<FaultSent, MailError> {
        let tx = self.mail_tx()?;
        purge_due(&tx, f.now_ms)?;
        let sent = send_fault_in(&tx, f, build)?;
        tx.commit()?;
        Ok(sent)
    }

    /// Clears `agent`'s faults that report `condition` (their data part's
    /// `clears`), now seen to end, in one mail transaction: for each code,
    /// acks its latest occurrence's deliveries, as mdkg, with `via:
    /// "clear"` and the outcome `cleared: <condition>`, and appends one
    /// `fault.clear` record per occurrence, even when every delivery was
    /// already acked (A26, R3). Faults reporting other conditions stay open.
    pub fn clear_faults(
        &mut self,
        agent: &str,
        condition: &str,
        now_ms: u64,
    ) -> Result<Vec<FaultCleared>, MailError> {
        let tx = self.mail_tx()?;
        purge_due(&tx, now_ms)?;
        let cleared = clear_in(&tx, agent, condition, now_ms)?;
        tx.commit()?;
        Ok(cleared)
    }

    /// How many occurrences of `agent`'s `code` were cleared: a login
    /// fault's occurrence counter, which moves only when a spell ends,
    /// whatever happened to the fault's deliveries (A26).
    pub fn fault_clears(&self, agent: &str, code: &str) -> Result<u64, MailError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM log WHERE record LIKE ?1
               AND json_extract(record, '$.kind') = ?2
               AND json_extract(record, '$.body.agent') = ?3
               AND json_extract(record, '$.body.code') = ?4",
            params![
                format!("%\"kind\":\"{CLEAR_RECORD}\"%"),
                CLEAR_RECORD,
                agent,
                code
            ],
            |r| r.get(0),
        )?;
        Ok(ms(n))
    }
}
