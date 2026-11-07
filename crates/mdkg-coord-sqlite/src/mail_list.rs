//! `msg list`: mail across mailboxes, by kind, read-only (edd-01a1a1f1,
//! L3). It opens the store file read-only, as the decision audit does, and
//! reads in one snapshot: no ack, no shown mark, no purge, no log record.
//! A body whose retention is up but that no command has deleted yet reads
//! as purged (`mail::DUE`, the condition the purge deletes by), so a list
//! answers the same whether or not a purge ran first.

use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value as Sql};

use crate::mail::{DUE, MailError, Message, message_by_id};
use crate::store::{Store, StoreError};

/// The most messages one page holds, and the default.
pub const LIST_MAX: usize = 200;
pub const LIST_DEFAULT: usize = 50;
/// A page stops before the body that would take it past this many bytes
/// (as stored); its first message is always returned.
pub const LIST_MAX_BYTES: u64 = 1024 * 1024;

/// Which deliveries' state a message is listed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListState {
    /// A delivery in scope is still unacked.
    #[default]
    Unacked,
    /// Every delivery in scope is acked (or there is none).
    Acked,
    Any,
}

impl ListState {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "unacked" => Some(Self::Unacked),
            "acked" => Some(Self::Acked),
            "any" => Some(Self::Any),
            _ => None,
        }
    }
}

/// What [`read_list`] selects.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Only these kinds (any of them); empty, every kind but `receipt`,
    /// including kinds this build doesn't know (as [`crate::mail::InboxQuery`]).
    pub kinds: Vec<String>,
    /// Mail delivered to these channels' current members, and posts to
    /// them.
    pub channels: Vec<String>,
    /// Mail delivered to these mailboxes.
    pub to: Vec<String>,
    /// Only mail from these senders; empty, every sender.
    pub senders: Vec<String>,
    pub state: ListState,
    /// Only messages after this sequence number.
    pub after: u64,
    /// 1 to [`LIST_MAX`].
    pub limit: usize,
    pub now_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    /// Oldest first; a due body is `None`, as a purged one.
    pub messages: Vec<Message>,
    /// The highest sequence returned, or `after` when none.
    pub cursor: u64,
    /// More matched than this page holds.
    pub more: bool,
    /// The event-stream head from the same snapshot; `None` with no store.
    pub event_cursor: Option<String>,
    /// The mailboxes the scope resolved to (empty: every mailbox).
    pub mailboxes: Vec<String>,
}

/// Lists mail across mailboxes from the store at `path`, without writing
/// anything there. A missing store has no mail (and no channels).
pub fn read_list(path: &Path, q: &ListQuery) -> Result<Listing, MailError> {
    let present = crate::store::store_file_present(path).map_err(StoreError::from)?;
    let conn = if present {
        Store::open_raw_read_only(path)?
    } else {
        None
    };
    let Some(conn) = conn else {
        if let Some(channel) = q.channels.first() {
            return Err(MailError::UnknownRoom(channel.clone()));
        }
        return Ok(Listing {
            messages: Vec::new(),
            cursor: q.after,
            more: false,
            event_cursor: None,
            mailboxes: q.to.clone(),
        });
    };
    // One snapshot for the scope, the rows and the head. A read-only
    // connection can't write, whatever runs on it.
    let tx = conn.unchecked_transaction()?;
    crate::store::check_mail_schema(&tx)?;
    let listing = list_on(&tx, q)?;
    tx.finish()?;
    Ok(listing)
}

fn list_on(conn: &Connection, q: &ListQuery) -> Result<Listing, MailError> {
    let mut mailboxes: BTreeSet<String> = q.to.iter().cloned().collect();
    for channel in &q.channels {
        if !room_exists(conn, channel)? {
            return Err(MailError::UnknownRoom(channel.clone()));
        }
        mailboxes.extend(members_of(conn, channel)?);
    }
    let scoped = !q.channels.is_empty() || !q.to.is_empty();
    let limit = q.limit.clamp(1, LIST_MAX);

    // ?1 is the time now (DUE's), ?2 `after`; the rest follow in order.
    let mut args: Vec<Sql> = vec![
        Sql::Integer(sql_ms(q.now_ms)),
        Sql::Integer(sql_ms(q.after)),
    ];
    let mut sql = String::from("SELECT m.seq FROM msg_messages m WHERE m.seq > ?2");
    if q.kinds.is_empty() {
        sql.push_str(" AND m.kind != 'receipt'");
    } else {
        sql.push_str(" AND m.kind IN ");
        push_list(&mut sql, &mut args, &q.kinds);
    }
    if !q.senders.is_empty() {
        sql.push_str(" AND m.sender IN ");
        push_list(&mut sql, &mut args, &q.senders);
    }
    let mailboxes: Vec<String> = mailboxes.into_iter().collect();
    // Deliveries in scope: to a scope mailbox, or every delivery unscoped.
    let mut in_scope = String::new();
    if scoped {
        in_scope.push_str(" AND d.mailbox IN ");
        push_list(&mut in_scope, &mut args, &mailboxes);
        sql.push_str(" AND (EXISTS (SELECT 1 FROM msg_deliveries d WHERE d.seq = m.seq");
        sql.push_str(&in_scope);
        sql.push(')');
        if !q.channels.is_empty() {
            sql.push_str(" OR m.room IN ");
            push_list(&mut sql, &mut args, &q.channels);
        }
        sql.push(')');
    }
    let open = format!(
        "EXISTS (SELECT 1 FROM msg_deliveries d WHERE d.seq = m.seq AND d.state != 'acked'{in_scope})"
    );
    match q.state {
        ListState::Unacked => sql.push_str(&format!(" AND {open}")),
        ListState::Acked => sql.push_str(&format!(" AND NOT {open}")),
        ListState::Any => {}
    }
    args.push(Sql::Integer(i64::try_from(limit + 1).unwrap_or(i64::MAX)));
    sql.push_str(&format!(" ORDER BY m.seq LIMIT ?{}", args.len()));
    // ?1 (DUE's) is unused here; SQLite counts parameters by the
    // highest index, so its slot is still bound.

    let ids: Vec<String> = {
        let mut stmt = conn.prepare(&sql.replacen("SELECT m.seq", "SELECT m.message_id", 1))?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut more = ids.len() > limit;
    let mut due = conn.prepare(&format!(
        "SELECT ({DUE}) FROM msg_messages m WHERE m.seq = ?2"
    ))?;
    let mut messages = Vec::new();
    let mut bytes = 0u64;
    for id in ids.iter().take(limit) {
        let Some(mut message) = message_by_id(conn, id)? else {
            continue;
        };
        let is_due: bool = due
            .query_row(params![sql_ms(q.now_ms), sql_ms(message.seq)], |r| r.get(0))
            .optional()?
            .unwrap_or(false);
        if is_due {
            message.parts = None;
        }
        if message.parts.is_some() {
            if !messages.is_empty() && bytes.saturating_add(message.size) > LIST_MAX_BYTES {
                more = true;
                break;
            }
            bytes = bytes.saturating_add(message.size);
        }
        messages.push(message);
    }
    let event_cursor = crate::events::head_in(conn)?.encode();
    let cursor = messages.last().map_or(q.after, |m| m.seq);
    Ok(Listing {
        messages,
        cursor,
        more,
        event_cursor: Some(event_cursor),
        mailboxes,
    })
}

/// Appends `(?a, ?b, ...)` binding `items`, or `(NULL)`, which matches
/// nothing, when there are none.
fn push_list(sql: &mut String, args: &mut Vec<Sql>, items: &[String]) {
    if items.is_empty() {
        sql.push_str("(NULL)");
        return;
    }
    sql.push('(');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        args.push(Sql::Text(item.clone()));
        sql.push_str(&format!("?{}", args.len()));
    }
    sql.push(')');
}

fn sql_ms(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn room_exists(conn: &Connection, room: &str) -> Result<bool, MailError> {
    Ok(conn
        .prepare("SELECT 1 FROM msg_rooms WHERE name = ?1")?
        .exists(params![room])?)
}

fn members_of(conn: &Connection, room: &str) -> Result<Vec<String>, MailError> {
    let mut stmt =
        conn.prepare("SELECT mailbox FROM msg_members WHERE room = ?1 ORDER BY mailbox")?;
    let rows = stmt.query_map(params![room], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}
