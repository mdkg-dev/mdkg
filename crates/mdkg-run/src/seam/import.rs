//! `import` (the edd's turn.import): the turn's final message becomes the
//! agent's reply, and the mail it was shown is acked. `NO_REPLY` acks it as
//! answered with nothing sent; a task-only turn (no mail shown) keeps the
//! message in its report; a reply the store refuses for what it says is
//! acked as refused, each message on its own (H36), and fails the turn.

use std::fs;
use std::path::Path;

use super::Turn;
use crate::error::RunError;
use crate::mail::{DigestImport, Import, Mail, Message, ReviewImport};
use crate::state;
use crate::turn::Outcome;

/// `import`'s request, in-process.
pub(crate) struct ImportRequest<'a> {
    /// The agent whose turn this is.
    pub(crate) agent: &'a str,
    /// Review requests actually delivered to this reviewer in this turn.
    pub(crate) review_requests: &'a [String],
    /// The final message, verbatim.
    pub(crate) reply: String,
    /// The mail the turn was shown, by id.
    pub(crate) shown: Vec<String>,
    /// The newest message shown, which the reply answers; `None` on a
    /// task-only turn.
    pub(crate) newest: Option<&'a Message>,
    pub(crate) digest_to: Option<&'a str>,
    pub(crate) digest_nodes: &'a [String],
    pub(crate) digest_answer: Option<&'a str>,
    pub(crate) digest_references: &'a [String],
    /// The resolved reply kind; unset applies the answered message's default.
    pub(crate) reply_kind: Option<&'a str>,
    /// The turn's number and its files' stem, where the reply is kept.
    pub(crate) number: u64,
    pub(crate) stem: &'a Path,
    /// The highest number of a message the agent had sent answering the
    /// newest message when the turn started ([`Mail::sent_to`]); `None`
    /// when that couldn't be read, and the reply is imported as before.
    pub(crate) sent_before: Option<u64>,
}

/// What `import` decided.
pub(crate) struct Imported {
    /// `replied`, `no_reply`, or `ran` for a task-only turn.
    pub(crate) outcome: Outcome,
    pub(crate) reply_id: Option<String>,
    /// A task-only turn's final message, kept rather than sent.
    pub(crate) reply: Option<String>,
}

/// The filter that refused a reply's content, by the import's error code;
/// `None` for any other failure (the store, the CLI), which leaves the
/// shown mail unacked for a later turn.
fn refused_reply(code: &str) -> Option<&'static str> {
    match code {
        "msg.secret_refused" => Some("the mail secret filter"),
        "msg.too_large" => Some("the mail size cap"),
        _ => None,
    }
}
/// Imports `req.reply` for `lifecycle`'s turn, and counts the acked mail
/// out of `shows`.
pub(crate) fn import(
    mail: &mut dyn Mail,
    dir: &Path,
    shows: &mut state::Shows,
    lifecycle: &mut Turn<'_, '_>,
    req: ImportRequest<'_>,
) -> Result<Imported, RunError> {
    let ImportRequest {
        agent: name,
        review_requests,
        reply,
        shown,
        newest,
        digest_to,
        digest_nodes,
        digest_answer,
        digest_references,
        reply_kind,
        number,
        stem,
        sent_before,
    } = req;
    // The turn already answered the newest message itself (a handoff or
    // findings to its sender, replying to it or in its context, by MCP or
    // the CLI): its final message isn't imported as a second reply, which
    // would wake the recipient twice. A question, note or other mail to the
    // sender doesn't count. The shown mail is acked as answered,
    // as for NO_REPLY, and the receipt says why (#20931).
    let already_mailed = match (newest, sent_before) {
        (Some(m), Some(before)) if !crate::digest::is_no_reply(&reply) => {
            mail.sent_to(name, m, before).is_ok_and(|top| top > before)
        }
        _ => false,
    };
    let mut done = Imported {
        outcome: Outcome::Ran,
        reply_id: None,
        reply: None,
    };
    match newest {
        // Nothing to say: no reply goes anywhere, and the mail shown is
        // acked as answered (friction 83), so it doesn't come back.
        Some(_) if already_mailed || crate::digest::is_no_reply(&reply) => {
            let outcome = if already_mailed {
                "answered: the turn already mailed its recipient".to_string()
            } else {
                format!("answered: nothing to say ({})", crate::digest::NO_REPLY)
            };
            for one in &shown {
                match mail.ack(name, std::slice::from_ref(one), &outcome) {
                    Ok(()) => {
                        if let Some(receipt) = lifecycle.receipt.as_mut() {
                            receipt.acknowledged.push(one.clone());
                        }
                    }
                    Err(e) if e.code == "msg.not_holder" => {
                        if let Some(receipt) = lifecycle.receipt.as_mut() {
                            receipt.not_held.push(one.clone());
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            done.outcome = Outcome::NoReply;
            // Acked: no longer counted. The ack stands if this can't be
            // written; acked mail never comes again.
            shows.acked(&shown);
            let _ = shows.save(dir, name);
            if already_mailed {
                // Not imported, so the mail filters never read it: the
                // retained receipt never carries it either (it could hold
                // what they would refuse). It's kept beside the receipt,
                // as a refused reply is (review of 8a8ff4b8).
                let reply_file = stem.with_extension("reply");
                fs::write(&reply_file, &reply)
                    .map_err(|e| RunError::io("can't write the reply", e))?;
                if let Some(receipt) = lifecycle.receipt.as_mut() {
                    receipt.summary = Some(format!(
                        "withheld: not imported, as the turn already mailed its recipient; \
                         the final message is kept in the launcher's turn-{number}.reply"
                    ));
                }
            }
            if let Some(receipt) = lifecycle.receipt.as_mut() {
                receipt.reply_status = match (already_mailed, receipt.not_held.is_empty()) {
                    (true, true) => {
                        "not imported: the turn already mailed its recipient; acknowledged"
                    }
                    (true, false) => {
                        "not imported: the turn already mailed its recipient; some shown mail was \
                         no longer held"
                    }
                    (false, true) => "no reply; acknowledged",
                    (false, false) => "no reply; some shown mail was no longer held",
                };
            }
        }
        Some(m) => {
            let reply_file = stem.with_extension("reply");
            fs::write(&reply_file, &reply).map_err(|e| RunError::io("can't write the reply", e))?;
            let imported = if let Some(to) = digest_to {
                mail.import_digest(&DigestImport {
                    kind: reply_kind.unwrap_or("handoff"),
                    from: name,
                    to,
                    nodes: digest_nodes,
                    answer: digest_answer,
                    references: digest_references,
                    file: &reply_file,
                })
            } else {
                mail.import(&Import {
                    kind: crate::turn::final_reply_kind(
                        std::iter::once(m),
                        reply_kind,
                        crate::turn::default_reply_kind(&m.kind),
                    ),
                    from: name,
                    reply_to: &m.message_id,
                    to: &m.sender,
                    // Always direct: a reply to a channel post goes to its
                    // sender alone, so the channel's members aren't woken
                    // by every member's answer (friction 83).
                    room: None,
                    // Never named: the store gives a direct reply to a
                    // direct message its context, and starts a new one for
                    // a direct reply to a room post. Named, a room post's
                    // `room:<name>` (or an older build's on a direct
                    // message) is refused as an id after the turn, and a
                    // direct reply could join a room's conversation (merge
                    // review of 5d9a828).
                    context: None,
                    file: &reply_file,
                })
            }
            .map_err(|e| {
                e.and_hint(format!(
                    "the reply is kept in {}",
                    crate::digest::clean(&reply_file.display().to_string())
                ))
            });
            let id = match imported {
                Ok(id) => id,
                Err(e) => match refused_reply(&e.code) {
                    // The store refused what the reply says, so another
                    // turn would answer the same request again
                    // (bug-01a17e86): it's acked as refused, naming the
                    // filter but never the text it matched.
                    Some(filter) => {
                        // What the turn wrote was refused: its shows stand.
                        lifecycle.own_failure = true;
                        if digest_to.is_some() {
                            return Err(e.and_hint("the digest mail stays unacknowledged; the reply is kept in the turn file"));
                        }
                        let outcome = format!(
                            "refused: the reply was refused by {filter} ({}); it is kept in \
                             the launcher's turn-{number}.reply",
                            e.code
                        );
                        // The receipt is retained and mailed: it never
                        // carries what the store refused (review of
                        // dfd9a4e).
                        if let Some(receipt) = lifecycle.receipt.as_mut() {
                            receipt.summary = Some(format!("withheld: {outcome}"));
                        }
                        let mut not_held = 0;
                        for one in &shown {
                            match mail.ack(name, std::slice::from_ref(one), &outcome) {
                                Ok(()) => {
                                    if let Some(receipt) = lifecycle.receipt.as_mut() {
                                        receipt.acknowledged.push(one.clone());
                                    }
                                }
                                Err(a) if a.code == "msg.not_holder" => {
                                    not_held += 1;
                                    if let Some(receipt) = lifecycle.receipt.as_mut() {
                                        receipt.not_held.push(one.clone());
                                    }
                                }
                                Err(a) => return Err(a),
                            }
                        }
                        let acked = if not_held == 0 {
                            "the shown mail was acked as refused"
                        } else {
                            "the shown mail still held was acked as refused; some was no \
                             longer held"
                        };
                        if let Some(receipt) = lifecycle.receipt.as_mut() {
                            receipt.reply_status = if not_held == 0 {
                                "refused; the shown mail was acked as refused"
                            } else {
                                "refused; some shown mail was no longer held"
                            };
                        }
                        return Err(e.and_hint(format!(
                            "{acked}; send the answer again by hand once it's fixed"
                        )));
                    }
                    None => return Err(e),
                },
            };
            if !review_requests.is_empty() {
                mail.attest_review_import(&ReviewImport {
                    reviewer: name,
                    turn: lifecycle.indexed.as_deref().unwrap_or(""),
                    message: &id,
                    requests: review_requests,
                    file: &reply_file,
                })?;
            }
            if let Some(receipt) = lifecycle.receipt.as_mut() {
                receipt.reply_id = Some(id.clone());
                receipt.reply_status = "imported; acknowledgements pending";
            }
            // One at a time: M1 refuses a whole ack when any id isn't in the
            // agent's mailboxes now (a role it no longer holds), and the rest
            // must still be acked.
            let outcome = format!("replied {id}");
            for one in &shown {
                match mail.ack(name, std::slice::from_ref(one), &outcome) {
                    Ok(()) => {
                        if let Some(receipt) = lifecycle.receipt.as_mut() {
                            receipt.acknowledged.push(one.clone());
                        }
                    }
                    Err(e) if e.code == "msg.not_holder" => {
                        if let Some(receipt) = lifecycle.receipt.as_mut() {
                            receipt.not_held.push(one.clone());
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            done.outcome = Outcome::Replied;
            // Acked: no longer counted. The ack stands if this can't be
            // written; acked mail never comes again.
            shows.acked(&shown);
            let _ = shows.save(dir, name);
            done.reply_id = Some(id);
            if let Some(receipt) = lifecycle.receipt.as_mut() {
                receipt.reply_status = if receipt.not_held.is_empty() {
                    "imported and acknowledged"
                } else {
                    "imported; some shown mail was no longer held"
                };
            }
        }
        None => done.reply = Some(reply),
    }
    Ok(done)
}
