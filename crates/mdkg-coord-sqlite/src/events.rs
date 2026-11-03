//! Read-only API event views. A follower holds no database handle between
//! polls, so a restore can proceed and the next poll checks the new history.
//! Imported log spans are never resolved against the local journal.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use mdkg_coord::{Command, Coordinator, Lease, Outcome, Status};
use mdkg_log::record::{Record, format_ts};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::{StoreError, log, mail, store};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    store: String,
    generation: u64,
    seq: u64,
    hash: String,
}

impl Cursor {
    pub fn encode(&self) -> String {
        format!(
            "c1.{}.{}.{}.{}",
            self.store, self.generation, self.seq, self.hash
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let parts: Vec<_> = text.split('.').collect();
        let ["c1", store, generation, seq, hash] = parts.as_slice() else {
            return None;
        };
        let hex = |s: &str, n| {
            s.len() == n
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        let number = |s: &str| {
            (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse::<u64>().ok())
                .flatten()
        };
        if !hex(store, 32) || !hex(hash, 16) {
            return None;
        }
        let seq = number(seq)?;
        if seq == 0 || seq > i64::MAX as u64 {
            return None;
        }
        Some(Self {
            store: (*store).into(),
            generation: number(generation)?,
            seq,
            hash: (*hash).into(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorProblem {
    Bad,
    Reset,
    Foreign,
    Expired,
}

#[derive(Debug)]
pub enum ReadError {
    Cursor {
        problem: CursorProblem,
        head: String,
    },
    Store(StoreError),
}

impl From<StoreError> for ReadError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}
impl From<rusqlite::Error> for ReadError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Store(value.into())
    }
}
impl From<serde_json::Error> for ReadError {
    fn from(value: serde_json::Error) -> Self {
        Self::Store(value.into())
    }
}

fn invalid(why: &str) -> StoreError {
    StoreError::Log(format!("event view: {why}"))
}

fn short_hash(hash: &str) -> Result<String, StoreError> {
    if !mdkg_log::record::valid_hash(hash) {
        return Err(invalid("invalid record hash"));
    }
    Ok(hash[7..23].into())
}

/// Call only inside the same transaction as the resource read it accompanies.
pub(crate) fn head_in(conn: &Connection) -> Result<Cursor, StoreError> {
    store::check_schema(conn)?;
    let store: String =
        conn.query_row("SELECT value FROM meta WHERE key = 'store_id'", [], |r| {
            r.get(0)
        })?;
    if store.len() != 32
        || !store
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(StoreError::BadMeta("store_id".into()));
    }
    let head = log::head(conn)?;
    if head != log::noted_head(conn)? {
        return Err(invalid("inconsistent log head"));
    }
    let (seq, hash) = head.ok_or_else(|| invalid("empty record chain"))?;
    Ok(Cursor {
        store,
        generation: store::restore_generation(conn)?,
        seq,
        hash: short_hash(&hash)?,
    })
}

fn check_cursor(conn: &Connection, cursor: &Cursor, head: &Cursor) -> Result<(), ReadError> {
    let problem = if cursor.store != head.store {
        Some(CursorProblem::Foreign)
    } else if cursor.generation != head.generation || cursor.seq > head.seq {
        Some(CursorProblem::Reset)
    } else {
        let hash: Option<String> = conn
            .query_row(
                "SELECT hash FROM log WHERE seq = ?1",
                [cursor.seq as i64],
                |r| r.get(0),
            )
            .optional()?;
        match hash {
            Some(hash) if short_hash(&hash)? == cursor.hash => None,
            _ => Some(CursorProblem::Reset),
        }
    };
    match problem {
        Some(problem) => Err(ReadError::Cursor {
            problem,
            head: head.encode(),
        }),
        None => Ok(()),
    }
}

/// A canonical RFC 3986 path segment. Encode bytes, so non-ASCII ids also
/// have exactly one spelling; hex digits in escapes are uppercase.
pub fn path(collection: &str, id: &str) -> String {
    format!("/v1/{collection}/{}", segment(id))
}

fn segment(id: &str) -> String {
    let mut out = String::new();
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(out, "%{byte:02X}").expect("writing a string");
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct Event {
    pub schema: &'static str,
    pub id: String,
    pub cursor: String,
    pub kind: String,
    pub time: String,
    pub actor: String,
    pub subjects: Vec<String>,
    pub durable: bool,
    pub data: Value,
}

impl Event {
    pub fn value(&self) -> Value {
        json!({"schema":self.schema,"id":self.id,"cursor":self.cursor,"type":self.kind,
            "time":self.time,"actor":self.actor,"subjects":self.subjects,
            "durable":self.durable,"data":self.data})
    }
}

#[derive(Debug)]
pub struct Page {
    pub events: Vec<Event>,
    pub head: String,
    pub more: bool,
}

#[derive(Debug, Clone)]
struct Replay {
    graph: String,
    nodes: BTreeMap<String, String>,
    coordinator: Coordinator,
    journal: i64,
    imported_through: u64,
    channels: BTreeMap<String, Channel>,
    /// The journal from before the log (up to genesis's `journal_seq`)
    /// didn't replay to its recorded outcomes, from this entry on, so the
    /// state it left can't be trusted: coordinator records stay plain
    /// record events, marked `opaque`.
    opaque: Option<i64>,
}

/// How [`Replay::apply_to`] treats an outcome today's coordinator doesn't
/// reproduce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Span {
    /// Records name these entries: any divergence is an error.
    Chained,
    /// Entries from before the log, written by older builds. One that
    /// differs only in the order of a list (0.7 ordered scopes by their
    /// lowercase form, so an old `observe_main` lists its leaves in
    /// another order) still replays; any other difference makes the
    /// coordinator's history opaque.
    PreChain,
}

#[derive(Debug, Clone)]
struct Channel {
    by: String,
    created_ms: u64,
    members: BTreeSet<String>,
    last_seq: u64,
}

/// A transient read transaction for every poll, with replay state retained
/// outside SQLite. No writes, schema upgrades, purges or mark-shown effects.
#[derive(Debug)]
pub struct Reader {
    path: PathBuf,
    position: Option<Cursor>,
    // Cursors publish a short anchor; internal chain links use the full hash.
    full_hash: Option<String>,
    origin: Cursor,
    replay: Replay,
}

fn read_only(path: &Path) -> Result<Connection, StoreError> {
    store::Store::open_read_only_connection(path)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "coordinator store is absent; run a writing command such as `{} msg inbox` to initialize it",
                mdkg_format::program::BIN
            ),
        )
        .into()
    })
}

/// A read-only head query for consumers which already own their resource
/// snapshot. For lease/status snapshots, use Store::coordinator_with_cursor.
pub fn head(path: &Path) -> Result<String, StoreError> {
    let conn = read_only(path)?;
    let tx = conn.unchecked_transaction()?;
    let head = head_in(&tx)?.encode();
    tx.finish()?;
    Ok(head)
}

impl Reader {
    /// None follows new records; `start` replays all history. Every other
    /// value must be an opaque cursor previously returned by this store.
    pub fn open(path: &Path, graph: &str, after: Option<&str>) -> Result<Self, ReadError> {
        let conn = read_only(path)?;
        let tx = conn.unchecked_transaction()?;
        let head = head_in(&tx)?;
        let target = match after {
            Some("start") => None,
            None => Some(head.clone()),
            Some(text) => {
                let cursor = Cursor::parse(text).ok_or_else(|| ReadError::Cursor {
                    problem: CursorProblem::Bad,
                    head: head.encode(),
                })?;
                check_cursor(&tx, &cursor, &head)?;
                Some(cursor)
            }
        };
        let ttl: String =
            tx.query_row("SELECT value FROM meta WHERE key = 'ttl'", [], |r| r.get(0))?;
        let ttl = ttl.parse().map_err(|_| StoreError::BadMeta("ttl".into()))?;
        let mut reader = Self {
            path: path.into(),
            position: None,
            full_hash: None,
            origin: head.clone(),
            replay: Replay {
                graph: graph.into(),
                nodes: BTreeMap::new(),
                coordinator: Coordinator::new(ttl),
                journal: 0,
                imported_through: 0,
                channels: BTreeMap::new(),
                opaque: None,
            },
        };
        if let Some(target) = &target {
            let mut stmt =
                tx.prepare("SELECT seq, hash, record FROM log WHERE seq <= ?1 ORDER BY seq")?;
            let mut rows = stmt.query([target.seq as i64])?;
            while let Some(row) = rows.next()? {
                let (record, hash) =
                    read_record(row, reader.position.as_ref(), reader.full_hash.as_deref())?;
                reader.replay.event(&tx, &record, &hash, &head)?;
                reader.position = Some(Cursor {
                    seq: record.seq,
                    hash: short_hash(&hash)?,
                    ..head.clone()
                });
                reader.full_hash = Some(hash);
            }
            if reader.position.as_ref() != Some(target) {
                return Err(invalid("history missing before cursor").into());
            }
        }
        tx.finish()?;
        Ok(reader)
    }

    pub fn cursor(&self) -> Option<String> {
        self.position.as_ref().map(Cursor::encode)
    }

    pub fn poll(&mut self, limit: usize) -> Result<Page, ReadError> {
        let conn = read_only(&self.path)?;
        let tx = conn.unchecked_transaction()?;
        let head = head_in(&tx)?;
        check_cursor(&tx, self.position.as_ref().unwrap_or(&self.origin), &head)?;
        if self.position.as_ref().is_some_and(|c| c.seq == head.seq) {
            tx.finish()?;
            return Ok(Page {
                events: Vec::new(),
                head: head.encode(),
                more: false,
            });
        }
        let mut replay = self.replay.clone();
        let mut position = self.position.clone();
        let mut full_hash = self.full_hash.clone();
        let mut events = Vec::new();
        let mut stmt =
            tx.prepare("SELECT seq, hash, record FROM log WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
        let mut rows = stmt.query(params![
            position.as_ref().map_or(0, |c| c.seq as i64),
            i64::try_from(limit.max(1)).unwrap_or(i64::MAX)
        ])?;
        while let Some(row) = rows.next()? {
            let (record, hash) = read_record(row, position.as_ref(), full_hash.as_deref())?;
            events.push(replay.event(&tx, &record, &hash, &head)?);
            position = Some(Cursor {
                seq: record.seq,
                hash: short_hash(&hash)?,
                ..head.clone()
            });
            full_hash = Some(hash);
        }
        let more = position.as_ref().is_none_or(|c| c.seq < head.seq);
        drop(rows);
        drop(stmt);
        tx.finish()?;
        self.position = position;
        self.full_hash = full_hash;
        self.replay = replay;
        Ok(Page {
            events,
            head: head.encode(),
            more,
        })
    }
}

fn read_record(
    row: &rusqlite::Row<'_>,
    before: Option<&Cursor>,
    previous_hash: Option<&str>,
) -> Result<(Record, String), StoreError> {
    let seq: i64 = row.get(0)?;
    let hash: String = row.get(1)?;
    let text: String = row.get(2)?;
    let (record, sealed) = Record::parse(&text).map_err(|e| StoreError::Log(e.to_string()))?;
    if record.seq != seq as u64 || hash != sealed {
        return Err(invalid("record differs from its index"));
    }
    let expected = before.map_or(1, |c| c.seq + 1);
    if record.seq != expected || record.prev.as_deref() != previous_hash {
        return Err(invalid("gap or broken link in event history"));
    }
    Ok((record, hash))
}

fn read_mail<T>(result: Result<T, mail::MailError>) -> Result<T, StoreError> {
    result.map_err(|e| match e {
        mail::MailError::Store(e) => e,
        _ => invalid("mail record unavailable"),
    })
}

impl Replay {
    /// Replays the journal up to `target`. Once the history is opaque,
    /// entries are read and checked for gaps but not applied, so `before`
    /// equals the state and no coordination event is derived.
    fn apply_to(
        &mut self,
        conn: &Connection,
        target: i64,
        span: Span,
    ) -> Result<Option<(Command, Outcome, Coordinator)>, StoreError> {
        if target < self.journal {
            return Err(invalid("journal position moved backwards"));
        }
        let mut stmt = conn.prepare(
            "SELECT seq, command, outcome FROM journal WHERE seq > ?1 AND seq <= ?2 ORDER BY seq",
        )?;
        let mut rows = stmt.query(params![self.journal, target])?;
        let mut last = None;
        while let Some(row) = rows.next()? {
            let seq: i64 = row.get(0)?;
            if seq != self.journal + 1 {
                return Err(invalid("journal gap"));
            }
            let command: Command = serde_json::from_str(&row.get::<_, String>(1)?)?;
            let outcome: Outcome = serde_json::from_str(&row.get::<_, String>(2)?)?;
            let before = self.coordinator.clone();
            if self.opaque.is_some() {
                self.journal = seq;
                last = Some((command, outcome, before));
                continue;
            }
            let replayed = self.coordinator.apply(&command);
            self.coordinator.drain_events();
            if replayed != outcome {
                match span {
                    Span::Chained => return Err(StoreError::ReplayDiverged(seq)),
                    Span::PreChain if same_but_order(replayed, outcome.clone()) => {}
                    Span::PreChain => {
                        self.opaque = Some(seq);
                        self.nodes.clear();
                        self.journal = seq;
                        last = Some((command, outcome, before));
                        continue;
                    }
                }
            }
            // An idempotent claim can return an older effective lease even
            // when the attempted node differs. Reconciliation can also clear
            // an old node association, so both paths use effective metadata.
            let effective = match (&command, &outcome) {
                (Command::Claim { .. }, Outcome::Lease { lease })
                | (
                    Command::Reconcile { .. },
                    Outcome::Reconciled {
                        lease,
                        status: Status::Active,
                    },
                ) => self.coordinator.lease(lease),
                _ => None,
            };
            if let Some(lease) = effective.filter(|l| l.status == Status::Active) {
                if let Some(node) = &lease.node {
                    self.nodes.insert(lease.scope.to_string(), node.clone());
                } else {
                    self.nodes.remove(lease.scope.as_str());
                }
            }
            self.journal = seq;
            last = Some((command, outcome, before));
        }
        if self.journal != target {
            return Err(invalid("missing journal entry"));
        }
        Ok(last)
    }

    fn event(
        &mut self,
        conn: &Connection,
        record: &Record,
        hash: &str,
        head: &Cursor,
    ) -> Result<Event, StoreError> {
        let mut event = Event {
            schema: "mdkg.api.event.v1",
            id: hash.into(),
            cursor: Cursor {
                seq: record.seq,
                hash: short_hash(hash)?,
                ..head.clone()
            }
            .encode(),
            kind: "mdkg.record.appended".into(),
            time: record.ts.clone(),
            actor: record.actor.clone(),
            subjects: vec![path("records", &record.seq.to_string())],
            durable: true,
            data: json!({"kind": record.kind, "record": record}),
        };
        if record.seq <= self.imported_through || record.kind.starts_with("legacy.") {
            event.data["imported"] = true.into();
            return Ok(event);
        }
        if record.kind == "log.import" {
            let count = record
                .body
                .get("count")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("import count missing"))?;
            self.imported_through = record
                .seq
                .checked_add(count)
                .ok_or_else(|| invalid("import span overflow"))?;
            if self.imported_through > head.seq {
                return Err(invalid("incomplete imported span"));
            }
            return Ok(event);
        }
        if record.kind == "log.genesis" {
            let journal = record
                .body
                .get("journal_seq")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid("genesis journal position missing"))?;
            // The journal the store had before its log began. A span that
            // doesn't replay says so here, once, and where it stopped.
            self.apply_to(conn, journal, Span::PreChain)?;
            if let Some(seq) = self.opaque {
                event.data["opaque"] = true.into();
                event.data["diverged_at"] = seq.into();
            }
            return Ok(event);
        }
        // Native fallback records still name the resources a client should
        // re-read, including a refused commit which ended its lease. Foreign
        // imported subjects above never acquire local resource identities.
        for subject in &record.subject {
            let resource = if let Some(id) = subject.strip_prefix("lease:") {
                Some(path("leases", id))
            } else if let Some(id) = subject.strip_prefix("changeset:") {
                Some(path("changesets", id))
            } else if let Some(id) = subject.strip_prefix("msg:") {
                Some(path("messages", id))
            } else if let Some(id) = subject.strip_prefix("room:") {
                Some(path("channels", id))
            } else if subject.starts_with("store:") {
                Some("/v1".into())
            } else {
                self.node_path_of_key(subject)
            };
            if let Some(resource) = resource {
                event.subjects.push(resource);
            }
        }
        event.subjects.sort();
        event.subjects.dedup();
        if log::COORDINATOR_KINDS.contains(&record.kind.as_str()) {
            let journal = record
                .body
                .get("journal")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid("record journal position missing"))?;
            if journal <= self.journal {
                return Err(invalid("duplicate local journal reference"));
            }
            let (command, outcome, before) = self
                .apply_to(conn, journal, Span::Chained)?
                .ok_or_else(|| invalid("record journal entry missing"))?;
            if record.body.get("command") != Some(&serde_json::to_value(&command)?)
                || record.body.get("outcome") != Some(&serde_json::to_value(&outcome)?)
            {
                return Err(invalid("record and journal disagree"));
            }
            event.data["refused"] = outcome.error().is_some().into();
            if let Some(error) = outcome.error() {
                event.data["code"] = refusal_code(error).into();
            }
            if self.opaque.is_some() {
                event.data["opaque"] = true.into();
            }
            let refused_claim = matches!((&command,&outcome),(Command::Claim{..},Outcome::Refused{error:mdkg_coord::Error::Held(id)}) if before.lease(id).is_none() && self.coordinator.lease(id).is_some_and(|l|l.status==Status::Conflict));
            if (outcome.error().is_none() || refused_claim) && before != self.coordinator {
                self.coordination_event(&mut event, &command, &outcome, &before)?;
            }
            return Ok(event);
        }
        match record.kind.as_str() {
            // The ledger's records (edd-01a1878e, "Events"): the node is the
            // subject, and the data is the entry as of the record (its
            // snapshot) or the settle record itself. A node id that's
            // missing or doesn't parse leaves the plain record event,
            // never a failed read (Terra's code review #10631, minor 2).
            "entry.open" | "entry.change" | "ledger.settle" => {
                let node = record.body.get("node").and_then(Value::as_str);
                if let Some(subject) = node.and_then(|n| self.node_path_of_key(n)) {
                    event.kind = match record.kind.as_str() {
                        "entry.open" => "mdkg.entry.opened",
                        "entry.change" => "mdkg.entry.changed",
                        _ => "mdkg.ledger.settled",
                    }
                    .into();
                    event.subjects = vec![subject.clone()];
                    event.data = json!({"node": subject, "record": record.body});
                }
            }
            "msg.send" => {
                store::check_mail_schema(conn)?;
                let id = body_str(record, "message_id")?;
                // v1 retains message headers and recipient rows permanently;
                // retention removes only msg_bodies. A missing header is store
                // damage, so stop rather than fabricate a historical message.
                let mut message = read_mail(mail::message_by_id(conn, id))?
                    .ok_or_else(|| invalid("sent message missing"))?;
                // The recipient set is immutable. Only body presence comes from
                // today's row; acknowledgments happened after this event.
                for delivery in &mut message.deliveries {
                    delivery.state = "delivered".into();
                    delivery.acked_by = None;
                    delivery.acked_via = None;
                    delivery.acked_ms = None;
                    delivery.outcome = None;
                }
                if let Some(channel) = &message.room
                    && let Some(state) = self.channels.get_mut(channel)
                {
                    state.last_seq = message.seq;
                }
                event.kind = "mdkg.message.sent".into();
                event.subjects = message_subjects(&message);
                event.data = json!({"message": message_value(&message)});
            }
            "msg.ack" => {
                let mut acked = Vec::new();
                let mut subjects = BTreeSet::new();
                for pair in body_array(record, "acked")? {
                    let id = pair
                        .get("message_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("ack message missing"))?;
                    let mailbox = pair
                        .get("mailbox")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("ack mailbox missing"))?;
                    let message = path("messages", id);
                    let mailbox = path("mailboxes", mailbox);
                    subjects.extend([message.clone(), mailbox.clone()]);
                    acked.push(json!({"message":message,"mailbox":mailbox}));
                }
                let purged = message_paths(body_array(record, "purged")?)?;
                subjects.extend(purged.iter().cloned());
                for pair in body_array(record, "acked")? {
                    if let Some(id) = pair.get("message_id").and_then(Value::as_str)
                        && let Some(message) = read_mail(mail::message_by_id(conn, id))?
                    {
                        subjects.extend(message_subjects(&message));
                    }
                }
                for id in strings(body_array(record, "purged")?)? {
                    if let Some(message) = read_mail(mail::message_by_id(conn, &id))? {
                        subjects.extend(message_subjects(&message));
                    }
                }
                event.kind = "mdkg.message.acked".into();
                event.subjects = subjects.into_iter().collect();
                event.data = json!({"acked":acked,"by":body_str(record,"by")?,"purged":purged});
            }
            "msg.purge" => {
                let messages = message_paths(body_array(record, "purged")?)?;
                event.kind = "mdkg.message.purged".into();
                let mut subjects: BTreeSet<String> = messages.iter().cloned().collect();
                for id in strings(body_array(record, "purged")?)? {
                    if let Some(message) = read_mail(mail::message_by_id(conn, &id))? {
                        subjects.extend(message_subjects(&message));
                    }
                }
                event.subjects = subjects.into_iter().collect();
                event.data = json!({"messages":messages,"reason":body_str(record,"reason")?});
            }
            "room.create" | "room.join" | "room.leave" => {
                let name = body_str(record, "room")?;
                if record.kind == "room.create" {
                    // Immutable metadata is not in old create records. The
                    // original channel row survives all membership changes.
                    store::check_mail_schema(conn)?;
                    let stored = read_mail(mail::room_on(conn, name))?;
                    self.channels.insert(
                        name.into(),
                        Channel {
                            by: stored.created_by,
                            created_ms: stored.created_ms,
                            members: strings(body_array(record, "members")?)?
                                .into_iter()
                                .collect(),
                            last_seq: 0,
                        },
                    );
                } else {
                    let Some(channel) = self.channels.get_mut(name) else {
                        // The channel may predate this store's log. Its current
                        // membership cannot establish a historical baseline.
                        return Ok(event);
                    };
                    if record.kind == "room.join" {
                        channel
                            .members
                            .extend(strings(body_array(record, "added")?)?);
                    } else {
                        for member in strings(body_array(record, "removed")?)? {
                            channel.members.remove(&member);
                        }
                    }
                }
                let channel = self
                    .channels
                    .get(name)
                    .ok_or_else(|| invalid("channel history missing"))?;
                event.kind = format!(
                    "mdkg.channel.{}",
                    match record.kind.as_str() {
                        "room.create" => "created",
                        "room.join" => "joined",
                        _ => "left",
                    }
                );
                event.subjects = vec![path("channels", name)];
                event.data = json!({"channel":{"uri":path("channels",name),"name":name,"createdBy":channel.by,"createdAt":format_ts(channel.created_ms),"members":channel.members,"defaultContext":format!("room:{name}"),"last_seq":channel.last_seq}});
            }
            _ => {}
        }
        Ok(event)
    }

    fn node_path(&self, leaf: &str) -> Option<String> {
        self.node_path_of_key(self.nodes.get(leaf)?)
    }

    fn node_path_of_key(&self, key: &str) -> Option<String> {
        let id = key.rsplit_once('#').map_or(key, |(_, id)| id);
        mdkg_format::id::NodeId::parse(id).ok()?;
        Some(format!(
            "{}/nodes/{}",
            path("graphs", &self.graph),
            segment(id)
        ))
    }

    fn coordination_event(
        &self,
        event: &mut Event,
        command: &Command,
        outcome: &Outcome,
        before: &Coordinator,
    ) -> Result<(), StoreError> {
        let mut leases = Vec::new();
        let kind = match (command, outcome) {
            (
                Command::Claim { .. },
                Outcome::Refused {
                    error: mdkg_coord::Error::Held(lease),
                },
            )
            | (Command::Claim { .. }, Outcome::Lease { lease }) => {
                let lease = self
                    .coordinator
                    .lease(lease)
                    .ok_or_else(|| invalid("claimed lease missing"))?;
                leases.push(lease);
                if lease.status == Status::Conflict {
                    "refused"
                } else {
                    "claimed"
                }
            }
            (Command::Heartbeat { lease, .. }, Outcome::Deadline { .. }) => {
                leases.push(
                    self.coordinator
                        .lease(lease)
                        .ok_or_else(|| invalid("renewed lease missing"))?,
                );
                "renewed"
            }
            (Command::Release { lease, .. }, Outcome::Done) => {
                leases.push(
                    self.coordinator
                        .lease(lease)
                        .ok_or_else(|| invalid("released lease missing"))?,
                );
                "released"
            }
            (Command::Expire { lease, .. }, Outcome::Done) => {
                leases.push(
                    self.coordinator
                        .lease(lease)
                        .ok_or_else(|| invalid("expired lease missing"))?,
                );
                "expired"
            }
            (Command::ExpireDue { .. }, Outcome::Expired { leases: ids }) => {
                for id in ids {
                    leases.push(
                        self.coordinator
                            .lease(id)
                            .ok_or_else(|| invalid("expired lease missing"))?,
                    );
                }
                "expired"
            }
            (Command::Reconcile { .. }, Outcome::Reconciled { lease, .. }) => {
                leases.push(
                    self.coordinator
                        .lease(lease)
                        .ok_or_else(|| invalid("reconciled lease missing"))?,
                );
                "reconciled"
            }
            _ => "",
        };
        if !leases.is_empty() {
            event.kind = format!("mdkg.lease.{kind}");
            event.subjects = leases
                .iter()
                .map(|l| path("leases", &l.id.to_string()))
                .collect();
            for lease in &leases {
                if lease.status != Status::Conflict {
                    if let Some(node) = lease
                        .node
                        .as_deref()
                        .and_then(|key| self.node_path_of_key(key))
                    {
                        event.subjects.push(node);
                    }
                    if lease.scope.as_str().starts_with("role:") {
                        event.subjects.push(path("mailboxes", lease.scope.as_str()));
                    }
                }
            }
            event.subjects.sort();
            event.subjects.dedup();
            let records: Vec<_> = leases
                .iter()
                .map(|l| lease_value(&self.coordinator, l))
                .collect();
            event.data = if matches!(command, Command::Expire { .. } | Command::ExpireDue { .. }) {
                json!({"leases":records})
            } else {
                json!({"lease":records[0]})
            };
            return Ok(());
        }
        match (command, outcome) {
            (Command::Commit { .. }, Outcome::Committed { changeset }) => {
                let changeset = self
                    .coordinator
                    .changesets()
                    .find(|c| &c.id == changeset)
                    .ok_or_else(|| invalid("committed changeset missing"))?;
                event.kind = "mdkg.changeset.committed".into();
                event.subjects = vec![
                    path("changesets", &changeset.id.to_string()),
                    path("leases", &changeset.lease.to_string()),
                ];
                let nodes: Vec<_> = changeset
                    .changes
                    .keys()
                    .filter_map(|leaf| self.node_path(leaf.as_str()))
                    .collect();
                event.subjects.extend(nodes.iter().cloned());
                event.data = json!({"changeset":changeset_value(changeset),"nodes":nodes});
            }
            (Command::Abandon { .. }, Outcome::Abandoned { changesets })
                if !changesets.is_empty() =>
            {
                let records: Vec<_> = changesets
                    .iter()
                    .map(|id| {
                        self.coordinator
                            .changesets()
                            .find(|c| &c.id == id)
                            .map(changeset_value)
                            .ok_or_else(|| invalid("abandoned changeset missing"))
                    })
                    .collect::<Result<_, _>>()?;
                event.kind = "mdkg.changeset.abandoned".into();
                event.subjects = changesets
                    .iter()
                    .map(|id| path("changesets", &id.to_string()))
                    .collect();
                event.data = json!({"changesets":records});
            }
            (Command::ObserveMain { .. }, Outcome::Observed { report }) => {
                let settled: Vec<_> = report
                    .committed
                    .iter()
                    .map(|id| path("changesets", &id.to_string()))
                    .collect();
                event.kind = "mdkg.main.observed".into();
                event.subjects = vec!["/v1".into()];
                event.subjects.extend(settled.iter().cloned());
                event.subjects.extend(
                    report
                        .conflicted
                        .iter()
                        .map(|id| path("changesets", &id.to_string())),
                );
                event.subjects.extend(
                    report
                        .external
                        .iter()
                        .filter_map(|leaf| self.node_path(leaf.as_str())),
                );
                event.subjects.sort();
                event.subjects.dedup();
                event.data = json!({"commit":null,"settled":settled});
            }
            (Command::SetWork { leaf, status, .. }, Outcome::Done) => {
                let old = before.work(leaf).map(|w| &w.status);
                if old != status.as_ref()
                    && let Some(node) = self.node_path(leaf.as_str())
                {
                    event.kind = "mdkg.node.status_changed".into();
                    event.subjects = vec![node.clone()];
                    event.data = json!({"node":node,"status":status});
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn body_str<'a>(record: &'a Record, key: &str) -> Result<&'a str, StoreError> {
    record
        .body
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("required event field missing"))
}
fn body_array<'a>(record: &'a Record, key: &str) -> Result<&'a [Value], StoreError> {
    record
        .body
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| invalid("required event list missing"))
}
fn strings(values: &[Value]) -> Result<Vec<String>, StoreError> {
    values
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid("event list contains a non-string"))
        })
        .collect()
}
fn message_paths(values: &[Value]) -> Result<Vec<String>, StoreError> {
    Ok(strings(values)?
        .iter()
        .map(|id| path("messages", id))
        .collect())
}

fn message_subjects(message: &mail::Message) -> Vec<String> {
    let mut subjects = vec![
        path("messages", &message.message_id),
        path("contexts", &message.context_id),
    ];
    subjects.extend(
        message
            .deliveries
            .iter()
            .map(|d| path("mailboxes", &d.mailbox)),
    );
    if let Some(channel) = &message.room {
        subjects.push(path("channels", channel));
    }
    subjects
}
fn message_value(m: &mail::Message) -> Value {
    let mut value = json!({"uri":path("messages",&m.message_id),"messageId":m.message_id,"seq":m.seq,"contextId":m.context_id,"kind":m.kind,"sender":m.sender,"to":m.to(),"body":if m.parts.is_some(){"present"}else{"purged"},"digest":m.digest,"size":m.size,"retention":m.retention,"sentAt":format_ts(m.sent_ms),"sentAtMs":m.sent_ms,"deliveries":m.deliveries.iter().map(|d|json!({"mailbox":d.mailbox,"state":"delivered"})).collect::<Vec<_>>()});
    if let Some(v) = &m.via {
        value["via"] = v.clone().into();
    }
    if let Some(v) = &m.room {
        value["channel"] = v.clone().into();
    }
    if !m.reference_ids.is_empty() {
        value["referenceIds"] = json!(m.reference_ids);
    }
    if let Some(v) = &m.parts {
        value["parts"] = v.clone();
    }
    if let Some(v) = &m.decision {
        value["decision"] = v.clone();
    }
    value
}
fn lease_value(state: &Coordinator, l: &Lease) -> Value {
    let mut value = json!({"uri":path("leases",&l.id.to_string()),"id":l.id.to_string(),"owner":l.owner,"scope":l.scope.as_str(),"status":l.status,"until":format_ts(l.deadline),"deadline_ms":l.deadline,"mode":l.mode});
    let others: BTreeSet<_> = state
        .holders(&l.scope)
        .filter(|h| {
            h.owner != l.owner
                && h.mode == mdkg_coord::Mode::Shared
                && l.mode == mdkg_coord::Mode::Shared
                && l.status == Status::Active
        })
        .map(|h| h.owner.clone())
        .collect();
    if !others.is_empty() {
        value["shared_with"] = json!(others);
    }
    if let Some(id) = l.changeset {
        value["changeset"] = id.to_string().into();
    }
    value
}
fn changeset_value(c: &mdkg_coord::Changeset) -> Value {
    let mut value = json!({"uri":path("changesets",&c.id.to_string()),"id":c.id.to_string(),"lease":c.lease.to_string(),"owner":c.owner,"scope":c.scope.as_str(),"state":c.state,"files":c.changes.keys().map(ToString::to_string).collect::<Vec<_>>()});
    if let Some(source) = &c.source {
        value["branch"] = source.branch.clone().into();
        value["commit"] = source.commit.clone().into();
    }
    if !c.depends_on.is_empty() {
        value["depends_on"] = json!(
            c.depends_on
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
    }
    value
}
/// Whether two outcomes hold the same lists in any order: what an entry
/// journaled before 0.7 ordered scopes by their lowercase form replays to.
fn same_but_order(a: Outcome, b: Outcome) -> bool {
    fn sorted(outcome: Outcome) -> Outcome {
        match outcome {
            Outcome::Observed { mut report } => {
                report.committed.sort();
                report.conflicted.sort();
                report.external.sort();
                Outcome::Observed { report }
            }
            Outcome::Expired { mut leases } => {
                leases.sort();
                Outcome::Expired { leases }
            }
            Outcome::Abandoned { mut changesets } => {
                changesets.sort();
                Outcome::Abandoned { changesets }
            }
            other => other,
        }
    }
    sorted(a) == sorted(b)
}

fn refusal_code(error: &mdkg_coord::Error) -> &'static str {
    use mdkg_coord::Error::*;
    match error {
        Held(_) => "coord.lease_held",
        CasFailed => "coord.cas_failed",
        PastDeadline => "coord.past_deadline",
        NotOwner => "coord.not_owner",
        NotActive | NotConfirmed => "coord.not_active",
        AlreadySettled(_) => "coord.already_settled",
        ObservationRaced(_) => "coord.observation_raced",
        KeyReused(_) => "coord.key_reused",
        DuplicateLease(_) => "coord.duplicate_lease",
        UnknownLease => "coord.unknown_lease",
        UnknownChangeset(_) => "coord.unknown_changeset",
        OutOfScope(_) => "coord.out_of_scope",
        Offline | AlreadyConfirmed | AlreadyRegistered(_) | NotDue => "coord.store_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Store, Upgrade};
    use mdkg_coord::{LeaseId, Mode, Request, Scope};

    const GRAPH: &str = "01a185ac-560e-76e0-91af-2c49dfc29b81";
    const NODE: &str = "task-01a185fc-9915-717f-b9bf-4486a3ef631b";

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("mdkg-api-events-{}-{time}-{n}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            let f = Self(path);
            drop(f.store());
            f
        }
        fn path(&self) -> PathBuf {
            self.0.join("coord.sqlite")
        }
        fn store(&self) -> Store {
            Store::open_with(&self.path(), 1000, Upgrade::Allow).unwrap()
        }
        fn reader(&self, after: Option<&str>) -> Reader {
            Reader::open(&self.path(), GRAPH, after).unwrap()
        }
        fn events(&self) -> Vec<Event> {
            let mut r = self.reader(Some("start"));
            let mut events = Vec::new();
            loop {
                let page = r.poll(2).unwrap();
                events.extend(page.events);
                if !page.more {
                    return events;
                }
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn event_reads_of_absent_stores_are_consistent_and_nonmutating() {
        let f = Fixture::new();
        let mut reader = f.reader(None);
        std::fs::remove_file(f.path()).unwrap();
        for kind in ["missing", "zero", "header"] {
            match kind {
                "zero" => {
                    std::fs::write(f.path(), b"").unwrap();
                    for suffix in ["-wal", "-shm", "-journal"] {
                        std::fs::write(f.0.join(format!("coord.sqlite{suffix}")), suffix).unwrap();
                    }
                }
                "header" => {
                    let conn = Connection::open(f.path()).unwrap();
                    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
                    drop(conn);
                }
                _ => {}
            }
            let snapshot = || {
                std::fs::read_dir(&f.0)
                    .unwrap()
                    .map(|entry| {
                        let entry = entry.unwrap();
                        (entry.file_name(), std::fs::read(entry.path()).unwrap())
                    })
                    .collect::<BTreeMap<_, _>>()
            };
            let before = snapshot();
            let absent = |error: StoreError| {
                assert!(
                    matches!(&error, StoreError::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
                    "{kind}: {error}"
                );
                assert!(error.to_string().contains("msg inbox"), "{error}");
            };
            absent(head(&f.path()).unwrap_err());
            for error in [
                Reader::open(&f.path(), GRAPH, None).unwrap_err(),
                reader.poll(1).unwrap_err(),
            ] {
                let ReadError::Store(error) = error else {
                    panic!("unexpected cursor error: {error:?}");
                };
                absent(error);
            }
            let after = snapshot();
            if kind == "header" {
                // Contract B: SQLite may create/rebuild WAL-index caches even
                // for a reader. The database's logical state stays unchanged.
                assert_eq!(
                    after.get(std::ffi::OsStr::new("coord.sqlite")),
                    before.get(std::ffi::OsStr::new("coord.sqlite"))
                );
            } else {
                assert_eq!(after, before, "{kind}");
            }
            for entry in std::fs::read_dir(&f.0).unwrap() {
                std::fs::remove_file(entry.unwrap().path()).unwrap();
            }
        }
    }

    /// edd-01a1878e, "Events": `entry.open`, `entry.change` and
    /// `ledger.settle` records map to `mdkg.entry.opened`,
    /// `mdkg.entry.changed` and `mdkg.ledger.settled`, the node their
    /// subject and the record their data; one naming no node id, or with
    /// no `node` string at all, stays a plain record event (Terra's
    /// #10631, minor 2).
    #[test]
    fn ledger_records_map_to_entry_and_settle_events() {
        let f = Fixture::new();
        let store = f.store();
        for (kind, node) in [
            ("entry.open", json!(NODE)),
            ("entry.change", json!(NODE)),
            ("ledger.settle", json!(NODE)),
            ("ledger.settle", json!("not a node")),
            ("entry.change", Value::Null),
            ("ledger.settle", json!(7)),
        ] {
            let mut body = json!({"node": node, "entry": "F1", "to": "open"});
            if node.is_null() {
                body.as_object_mut().unwrap().remove("node");
            }
            let refs = node
                .as_str()
                .map(|n| vec![mdkg_log::record::node_ref(n)])
                .unwrap_or_default();
            crate::log::append(
                &store.conn,
                "rev",
                kind,
                refs,
                body.as_object().unwrap().clone(),
            )
            .unwrap();
        }
        let events = f.events();
        let tail: Vec<&Event> = events.iter().rev().take(6).rev().collect();
        let subject = format!("/v1/graphs/{GRAPH}/nodes/{NODE}");
        for (event, kind) in tail.iter().zip([
            "mdkg.entry.opened",
            "mdkg.entry.changed",
            "mdkg.ledger.settled",
        ]) {
            assert_eq!(event.kind, kind);
            assert_eq!(event.subjects, vec![subject.clone()]);
            assert_eq!(event.data["node"], subject.as_str());
            assert_eq!(event.data["record"]["entry"], "F1");
        }
        for event in &tail[3..] {
            assert_eq!(event.kind, "mdkg.record.appended", "{event:?}");
        }
    }

    fn claim(owner: &str, scope: &str, key: Option<&str>) -> Command {
        Command::Claim {
            owner: owner.into(),
            scope: Scope::new(scope),
            mode: Mode::Exclusive,
            now: 0,
            request: key.map(|key| Request {
                key: key.into(),
                valid_until: 1000,
            }),
            node: None,
        }
    }
    fn id(outcome: Outcome) -> LeaseId {
        match outcome {
            Outcome::Lease { lease } => lease,
            other => panic!("not a lease: {other:?}"),
        }
    }
    fn sent(name: &str) -> mail::Outgoing {
        mail::Outgoing {
            message_id: name.into(),
            context_id: None,
            kind: "note".into(),
            sender: "a".into(),
            via: None,
            to: mail::To::Mailboxes(vec!["b".into()]),
            reference_ids: vec![],
            parts: json!([{"kind":"text","text":"untrusted fixture body"}]),
            retention: mail::Retention::PurgeOnAck,
            sender_mailboxes: vec!["a".into()],
            human_hold_days: 7,
            now_ms: 1,
        }
    }

    #[test]
    fn history_is_one_event_per_record_with_lease_states_refusals_and_noops() {
        let f = Fixture::new();
        let mut store = f.store();
        let lease = id(store.execute(&claim("a", "file", Some("once"))).unwrap());
        store.execute(&claim("a", "file", Some("once"))).unwrap();
        let conflict = match store.execute(&claim("b", "file", None)).unwrap() {
            Outcome::Refused {
                error: mdkg_coord::Error::Held(lease),
            } => lease,
            other => panic!("expected conflict, got {other:?}"),
        };
        store
            .execute(&Command::Release {
                owner: "b".into(),
                lease: lease.clone(),
                now: 1,
            })
            .unwrap();
        store
            .execute(&Command::Heartbeat {
                owner: "a".into(),
                lease: lease.clone(),
                now: 100,
            })
            .unwrap();
        store
            .execute(&Command::Release {
                owner: "a".into(),
                lease: lease.clone(),
                now: 101,
            })
            .unwrap();
        store
            .execute(&Command::Heartbeat {
                owner: "a".into(),
                lease,
                now: 102,
            })
            .unwrap();
        let events = f.events();
        assert_eq!(events.len(), store.log_head().unwrap().0 as usize);
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            [
                "mdkg.record.appended",
                "mdkg.lease.claimed",
                "mdkg.record.appended",
                "mdkg.lease.refused",
                "mdkg.record.appended",
                "mdkg.lease.renewed",
                "mdkg.lease.released",
                "mdkg.record.appended"
            ]
        );
        assert_eq!(events[1].data["lease"]["status"], "active");
        assert_eq!(events[1].data["lease"]["deadline_ms"], 1000);
        assert_eq!(events[2].data["refused"], false);
        assert_eq!(events[3].data["lease"]["id"], conflict.to_string());
        assert_eq!(events[3].data["lease"]["status"], "conflict");
        assert_eq!(events[4].data["refused"], true);
        assert_eq!(events[4].data["code"], "coord.not_owner");
        assert_eq!(events[5].data["lease"]["deadline_ms"], 1100);
        assert_eq!(events[6].data["lease"]["status"], "released");
        assert_eq!(events[7].data["refused"], true);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(Cursor::parse(&event.cursor).unwrap().seq, index as u64 + 1);
            assert!(event.durable);
        }
        let replay = f.events();
        assert_eq!(
            events.iter().map(|e| e.value()).collect::<Vec<_>>(),
            replay.iter().map(|e| e.value()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn channel_membership_and_message_deliveries_are_historical_bodies_are_current() {
        let f = Fixture::new();
        let mut store = f.store();
        store.room_create("dev", "a", &["b".into()], 1).unwrap();
        let mut out = sent("message-1");
        out.to = mail::To::Room("dev".into());
        store.send(&out).unwrap();
        store
            .room_join("dev", &["c".into()], "a", &["a".into()], 3)
            .unwrap();
        store
            .room_leave("dev", &["b".into()], "a", &["a".into()], false)
            .unwrap();
        let message = store.message("message-1", 4).unwrap();
        store
            .mark_shown(&[mail::Shown::of(&message)], &["b".into()], 4)
            .unwrap();
        store
            .ack(&["message-1".into()], &["b".into()], "b", None, None, 5)
            .unwrap();
        let events = f.events();
        assert_eq!(events[1].data["channel"]["members"], json!(["a", "b"]));
        assert_eq!(events[1].data["channel"]["last_seq"], 0);
        let message = &events[2].data["message"];
        assert_eq!(message["channel"], "dev");
        assert!(
            message.get("room").is_none(),
            "new API records use channel only"
        );
        assert_eq!(message["to"], json!(["b"]));
        assert_eq!(
            message["deliveries"],
            json!([{"mailbox":"b","state":"delivered"}])
        );
        assert_eq!(message["body"], "purged");
        assert!(message.get("parts").is_none());
        assert_eq!(events[3].data["channel"]["members"], json!(["a", "b", "c"]));
        assert_eq!(events[3].data["channel"]["last_seq"], 1);
        assert_eq!(events[4].data["channel"]["members"], json!(["a", "c"]));
        assert_eq!(
            events[5].data["acked"][0],
            json!({"message":"/v1/messages/message-1","mailbox":"/v1/mailboxes/b"})
        );
    }

    #[test]
    fn resume_and_replay_preserve_the_same_sequence() {
        let f = Fixture::new();
        let mut store = f.store();
        for n in 0..7 {
            store
                .execute(&claim("a", &format!("file-{n}"), None))
                .unwrap();
        }
        let all = f.events();
        for split in 0..all.len() {
            let mut resumed = f.reader(Some(&all[split].cursor));
            let page = resumed.poll(100).unwrap();
            assert!(!page.more);
            assert_eq!(
                all[split + 1..]
                    .iter()
                    .map(Event::value)
                    .collect::<Vec<_>>(),
                page.events.iter().map(Event::value).collect::<Vec<_>>()
            );
        }
        let mut live = f.reader(None);
        assert!(live.poll(100).unwrap().events.is_empty());
        store.execute(&claim("a", "later", None)).unwrap();
        assert_eq!(live.poll(100).unwrap().events.len(), 1);
    }

    #[test]
    fn cursors_reject_malformed_foreign_generation_past_head_and_forked_hash() {
        let f = Fixture::new();
        let good = head(&f.path()).unwrap();
        let parsed = Cursor::parse(&good).unwrap();
        for bad in [
            "",
            "start.",
            "c1.abc.0.1.abcdef0123456789",
            "c1.12345678901234567890123456789012.0.0.abcdef0123456789",
        ] {
            assert!(matches!(
                Reader::open(&f.path(), GRAPH, Some(bad)),
                Err(ReadError::Cursor {
                    problem: CursorProblem::Bad,
                    ..
                })
            ));
        }
        let mutations = [
            (
                Cursor {
                    store: format!(
                        "{}{}",
                        &parsed.store[..8],
                        if parsed.store[8..] == "a".repeat(24) {
                            "b".repeat(24)
                        } else {
                            "a".repeat(24)
                        }
                    ),
                    ..parsed.clone()
                },
                CursorProblem::Foreign,
            ),
            (
                Cursor {
                    generation: parsed.generation + 1,
                    ..parsed.clone()
                },
                CursorProblem::Reset,
            ),
            (
                Cursor {
                    seq: parsed.seq + 1,
                    ..parsed.clone()
                },
                CursorProblem::Reset,
            ),
            (
                Cursor {
                    hash: if parsed.hash == "a".repeat(16) {
                        "b".repeat(16)
                    } else {
                        "a".repeat(16)
                    },
                    ..parsed.clone()
                },
                CursorProblem::Reset,
            ),
        ];
        for (cursor, expected) in mutations {
            match Reader::open(&f.path(), GRAPH, Some(&cursor.encode())) {
                Err(ReadError::Cursor { problem, head }) => {
                    assert_eq!(problem, expected);
                    assert_eq!(head, good);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn restore_and_manual_copy_over_invalidate_open_followers_and_resume() {
        for native in [false, true] {
            let f = Fixture::new();
            let backup = crate::snapshot(&f.path(), "before-events").unwrap();
            let cursor = {
                let mut store = f.store();
                store.execute(&claim("a", "later", None)).unwrap();
                head(&f.path()).unwrap()
            };
            let mut follower = f.reader(Some(&cursor));
            if native {
                crate::restore(&f.path(), &backup).unwrap();
            } else {
                std::fs::copy(&backup, f.path()).unwrap();
            }
            assert!(matches!(
                follower.poll(10),
                Err(ReadError::Cursor {
                    problem: CursorProblem::Reset,
                    ..
                })
            ));
            assert!(matches!(
                Reader::open(&f.path(), GRAPH, Some(&cursor)),
                Err(ReadError::Cursor {
                    problem: CursorProblem::Reset,
                    ..
                })
            ));
        }
    }

    #[test]
    fn imports_with_local_journal_numbers_never_become_native_change_events() {
        let f = Fixture::new();
        let mut store = f.store();
        store.execute(&claim("a", "local", None)).unwrap();
        let records = [
            log::Imported {
                ts_ms: 1,
                actor: "elsewhere".into(),
                kind: "lease.claim".into(),
                subject: vec!["lease:lease-1".into()],
                body: serde_json::from_value(json!({"journal":1,"command":{"op":"anything"}}))
                    .unwrap(),
            },
            log::Imported {
                ts_ms: 2,
                actor: "elsewhere".into(),
                kind: "room.create".into(),
                subject: vec!["room:elsewhere".into()],
                body: serde_json::from_value(json!({"room":"elsewhere"})).unwrap(),
            },
        ];
        store.import_records("import", &records).unwrap();
        store.execute(&claim("a", "after-import", None)).unwrap();
        let events = f.events();
        let imported: Vec<_> = events
            .iter()
            .filter(|e| e.data["imported"] == true)
            .collect();
        assert_eq!(imported.len(), 2);
        assert!(imported.iter().all(|e| e.kind == "mdkg.record.appended"));
        assert_eq!(events.last().unwrap().kind, "mdkg.lease.claimed");
        let mut resumed = f.reader(Some(&imported[0].cursor));
        let page = resumed.poll(20).unwrap();
        assert_eq!(page.events[0].data["imported"], true);
        assert_eq!(page.events[1].kind, "mdkg.lease.claimed");
    }

    #[test]
    fn historical_node_proof_is_required_and_paths_are_canonical() {
        let f = Fixture::new();
        let mut store = f.store();
        let leaf = Scope::new(".mdkg/work/task.md");
        store
            .execute(&Command::SetWork {
                leaf: leaf.clone(),
                status: Some("review".into()),
                changeset: None,
                actor: "a".into(),
                now: 0,
            })
            .unwrap();
        let mut command = claim("a", leaf.as_str(), None);
        if let Command::Claim { node, .. } = &mut command {
            *node = Some(format!(".mdkg/work#{NODE}"));
        }
        store.execute(&command).unwrap();
        store
            .execute(&Command::SetWork {
                leaf: leaf.clone(),
                status: Some("blocked".into()),
                changeset: None,
                actor: "a".into(),
                now: 1,
            })
            .unwrap();
        store
            .execute(&Command::SetWork {
                leaf,
                status: Some("blocked".into()),
                changeset: None,
                actor: "a".into(),
                now: 2,
            })
            .unwrap();
        let events = f.events();
        assert_eq!(events[1].kind, "mdkg.record.appended");
        assert_eq!(events[3].kind, "mdkg.node.status_changed");
        assert_eq!(
            events[3].data["node"],
            format!("/v1/graphs/{GRAPH}/nodes/{NODE}")
        );
        assert_eq!(events[4].kind, "mdkg.record.appended");
        assert_eq!(path("actors", "human:nick"), "/v1/actors/human%3Anick");
        assert_eq!(
            path("messages", "colon:slash/%"),
            "/v1/messages/colon%3Aslash%2F%25"
        );
    }

    #[test]
    fn reads_do_not_append_records_or_change_bodies() {
        let f = Fixture::new();
        let mut store = f.store();
        store.send(&sent("read-only")).unwrap();
        let before = store.log_head().unwrap();
        let mut r = f.reader(Some("start"));
        r.poll(100).unwrap();
        head(&f.path()).unwrap();
        store.coordinator_with_cursor().unwrap();
        let inbox = store
            .inbox(&mail::InboxQuery {
                mailboxes: vec!["b".into()],
                now_ms: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(head(&f.path()).unwrap(), inbox.event_cursor);
        assert_eq!(before, store.log_head().unwrap());
        assert!(store.message("read-only", 1).unwrap().parts.is_some());
    }
    #[test]
    fn list_then_watch_has_no_gap_or_overlap_under_varied_writer_interleavings() {
        for seed in 1_u64..=16 {
            let f = Fixture::new();
            let path = f.path();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let start = barrier.clone();
            let writer = std::thread::spawn(move || {
                let mut store = Store::open_with(&path, 1000, Upgrade::Refuse).unwrap();
                start.wait();
                let mut random = seed;
                for n in 0..8 {
                    random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                    for _ in 0..random % 11 {
                        std::thread::yield_now();
                    }
                    store.send(&sent(&format!("concurrent-{n}"))).unwrap();
                }
            });
            let mut store = f.store();
            barrier.wait();
            for _ in 0..seed % 7 {
                std::thread::yield_now();
            }
            let query = mail::InboxQuery {
                mailboxes: vec!["b".into()],
                all: true,
                now_ms: 1,
                ..Default::default()
            };
            let listed = store.inbox(&query).unwrap();
            writer.join().unwrap();
            let mut stream = f.reader(Some(&listed.event_cursor));
            let page = stream.poll(100).unwrap();
            assert!(!page.more);
            let mut ids: BTreeSet<_> = listed
                .messages
                .iter()
                .map(|m| m.message_id.clone())
                .collect();
            for event in page.events {
                if event.kind == "mdkg.message.sent" {
                    assert!(
                        ids.insert(event.data["message"]["messageId"].as_str().unwrap().into()),
                        "duplicate across list/stream seed {seed}"
                    );
                }
            }
            let fresh = store.inbox(&query).unwrap();
            assert_eq!(
                ids,
                fresh
                    .messages
                    .iter()
                    .map(|m| m.message_id.clone())
                    .collect(),
                "gap in seed {seed}"
            );
            assert_eq!(ids.len(), 8);
        }
    }

    #[test]
    fn changesets_expiry_reconciliation_and_unknown_records_have_historical_payloads() {
        let f = Fixture::new();
        let mut store = f.store();
        let leaf = Scope::new(".mdkg/work/task.md");
        store
            .execute(&Command::Register {
                leaf: leaf.clone(),
                content: "before".into(),
            })
            .unwrap();
        let mut command = claim("a", leaf.as_str(), None);
        if let Command::Claim { node, .. } = &mut command {
            *node = Some(format!(".mdkg/work#{NODE}"));
        }
        let lease = id(store.execute(&command).unwrap());
        let committed = store
            .execute(&Command::Commit {
                owner: "a".into(),
                lease,
                changes: BTreeMap::from([(leaf.clone(), Some("after".into()))]),
                source: None,
                now: 1,
                landed: false,
            })
            .unwrap();
        let cs = match committed {
            Outcome::Committed { changeset } => changeset,
            other => panic!("{other:?}"),
        };
        let generation = store.coordinator().unwrap().generation();
        store
            .execute(&Command::ObserveMain {
                observation: mdkg_coord::Observation {
                    read_generation: generation,
                    leaves: BTreeMap::from([(leaf.clone(), "after".into())]),
                    merged: BTreeSet::new(),
                    prefix: None,
                },
            })
            .unwrap();
        let abandoned_lease = id(store.execute(&claim("a", "abandon", None)).unwrap());
        let abandoned = store
            .execute(&Command::Commit {
                owner: "a".into(),
                lease: abandoned_lease,
                changes: BTreeMap::from([(Scope::new("abandon"), Some("other".into()))]),
                source: None,
                now: 2,
                landed: false,
            })
            .unwrap();
        if let Outcome::Committed { changeset } = abandoned {
            store.execute(&Command::Abandon { changeset }).unwrap();
        } else {
            panic!("{abandoned:?}");
        }
        store.execute(&claim("a", "expires", None)).unwrap();
        store.execute(&Command::ExpireDue { now: 1000 }).unwrap();
        let state = store.coordinator().unwrap();
        let mut offline = mdkg_coord::Agent::new("offline", &state);
        offline.disconnect(&state);
        offline
            .claim_offline(&Scope::new("reconcile"), 1001, 1000)
            .unwrap();
        for lease in offline.provisional() {
            store
                .execute(&Command::Reconcile {
                    lease: Box::new(lease.clone()),
                    now: 1002,
                })
                .unwrap();
        }
        log::append(
            &store.conn,
            "future",
            "future.operation",
            vec![],
            serde_json::Map::new(),
        )
        .unwrap();
        let events = f.events();
        let committed = events
            .iter()
            .find(|e| {
                e.kind == "mdkg.changeset.committed" && e.data["changeset"]["id"] == cs.to_string()
            })
            .unwrap();
        assert_eq!(committed.data["changeset"]["state"], "sequenced");
        assert_eq!(committed.data["changeset"]["files"], json!([leaf.as_str()]));
        assert_eq!(
            committed.data["nodes"],
            json!([format!("/v1/graphs/{GRAPH}/nodes/{NODE}")])
        );
        let main = events
            .iter()
            .find(|e| e.kind == "mdkg.main.observed")
            .unwrap();
        assert!(main.data["commit"].is_null());
        assert_eq!(
            main.data["settled"],
            json!([path("changesets", &cs.to_string())])
        );
        let abandoned = events
            .iter()
            .find(|e| e.kind == "mdkg.changeset.abandoned")
            .unwrap();
        assert_eq!(abandoned.data["changesets"][0]["state"], "abandoned");
        let expired = events
            .iter()
            .find(|e| e.kind == "mdkg.lease.expired")
            .unwrap();
        assert!(
            expired.data["leases"]
                .as_array()
                .unwrap()
                .iter()
                .all(|l| l["status"] == "expired")
        );
        let reconciled = events
            .iter()
            .find(|e| e.kind == "mdkg.lease.reconciled")
            .unwrap();
        assert_eq!(reconciled.data["lease"]["status"], "active");
        assert_eq!(events.last().unwrap().kind, "mdkg.record.appended");
        assert_eq!(events.last().unwrap().data["kind"], "future.operation");
    }

    #[test]
    fn restoring_the_identical_anchor_still_changes_the_generation() {
        let f = Fixture::new();
        let cursor = head(&f.path()).unwrap();
        let backup = crate::snapshot(&f.path(), "same-anchor").unwrap();
        let mut follower = f.reader(Some(&cursor));
        crate::restore(&f.path(), &backup).unwrap();
        assert!(matches!(
            follower.poll(10),
            Err(ReadError::Cursor {
                problem: CursorProblem::Reset,
                ..
            })
        ));
        let after = Cursor::parse(&head(&f.path()).unwrap()).unwrap();
        let before = Cursor::parse(&cursor).unwrap();
        assert_eq!(after.seq, before.seq);
        assert_eq!(after.hash, before.hash);
        assert_ne!(after.generation, before.generation);
    }
    #[test]
    fn channels_which_predate_the_log_keep_membership_opaque_but_messages_complete() {
        let f = Fixture::new();
        let mut store = f.store();
        store
            .conn
            .execute(
                "INSERT INTO msg_rooms(name,created_by,created_ms) VALUES ('old','a',0)",
                [],
            )
            .unwrap();
        for member in ["a", "b"] {
            store.conn.execute("INSERT INTO msg_members(room,mailbox,added_by,added_ms) VALUES ('old',?1,'a',0)",[member]).unwrap();
        }
        store
            .room_join("old", &["c".into()], "a", &["a".into()], 1)
            .unwrap();
        let mut out = sent("old-channel-message");
        out.to = mail::To::Room("old".into());
        store.send(&out).unwrap();
        let events = f.events();
        assert_eq!(events[1].kind, "mdkg.record.appended");
        assert_eq!(events[1].data["kind"], "room.join");
        assert!(events[1].subjects.contains(&path("channels", "old")));
        assert_eq!(events[2].kind, "mdkg.message.sent");
        assert_eq!(events[2].data["message"]["to"], json!(["b", "c"]));
    }
    #[test]
    fn a_reused_claim_key_keeps_the_effective_leases_node_identity() {
        let f = Fixture::new();
        let mut store = f.store();
        let leaf = Scope::new(".mdkg/work/task.md");
        let mut original = claim("a", leaf.as_str(), Some("claim-once"));
        if let Command::Claim { node, .. } = &mut original {
            *node = Some(format!(".mdkg/work#{NODE}"));
        }
        let first = store.execute(&original).unwrap();
        let mut attempted = original.clone();
        if let Command::Claim { node, .. } = &mut attempted {
            *node = Some(".mdkg/work#task-01a185fc-9915-717f-b9bf-4486a3ef631c".into());
        }
        assert_eq!(
            store.execute(&attempted).unwrap(),
            first,
            "key returns the original effective lease"
        );
        store
            .execute(&Command::SetWork {
                leaf,
                status: Some("review".into()),
                changeset: None,
                actor: "a".into(),
                now: 1,
            })
            .unwrap();
        let events = f.events();
        assert_eq!(events[2].kind, "mdkg.record.appended");
        let event = events.last().unwrap();
        assert_eq!(event.kind, "mdkg.node.status_changed");
        assert_eq!(
            event.data["node"],
            format!("/v1/graphs/{GRAPH}/nodes/{NODE}")
        );
    }

    #[test]
    fn an_effective_node_less_reconciliation_clears_stale_node_proof() {
        let f = Fixture::new();
        let mut store = f.store();
        let leaf = Scope::new(".mdkg/work/task.md");
        let mut original = claim("a", leaf.as_str(), None);
        if let Command::Claim { node, .. } = &mut original {
            *node = Some(format!(".mdkg/work#{NODE}"));
        }
        let lease = id(store.execute(&original).unwrap());
        store
            .execute(&Command::Release {
                owner: "a".into(),
                lease,
                now: 1,
            })
            .unwrap();
        let state = store.coordinator().unwrap();
        let mut offline = mdkg_coord::Agent::new("a", &state);
        offline.disconnect(&state);
        offline.claim_offline(&leaf, 2, 1000).unwrap();
        for lease in offline.provisional() {
            assert!(lease.node.is_none());
            assert!(matches!(
                store
                    .execute(&Command::Reconcile {
                        lease: Box::new(lease.clone()),
                        now: 3
                    })
                    .unwrap(),
                Outcome::Reconciled {
                    status: Status::Active,
                    ..
                }
            ));
        }
        store
            .execute(&Command::SetWork {
                leaf,
                status: Some("review".into()),
                changeset: None,
                actor: "a".into(),
                now: 4,
            })
            .unwrap();
        let events = f.events();
        let event = events.last().unwrap();
        assert_eq!(event.kind, "mdkg.record.appended");
        assert_eq!(event.data["kind"], "work.status");
        assert!(event.data.get("node").is_none());
    }

    #[test]
    fn a_resealed_link_with_the_same_short_prefix_fails_full_chain_validation() {
        let f = Fixture::new();
        let mut store = f.store();
        let before = head(&f.path()).unwrap();
        store.execute(&claim("a", "file", None)).unwrap();
        let text: String = store
            .conn
            .query_row("SELECT record FROM log WHERE seq=2", [], |r| r.get(0))
            .unwrap();
        let (mut record, _) = Record::parse(&text).unwrap();
        let previous = record.prev.as_ref().unwrap();
        let mut forged = previous.clone();
        let last = forged.pop().unwrap();
        forged.push(if last == '0' { '1' } else { '0' });
        assert_eq!(short_hash(previous).unwrap(), short_hash(&forged).unwrap());
        assert_ne!(previous, &forged);
        record.prev = Some(forged);
        let (text, hash) = record.seal().unwrap();
        store
            .conn
            .execute(
                "UPDATE log SET hash=?1,record=?2 WHERE seq=2",
                params![hash, text],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE meta SET value=?1 WHERE key='log_head'",
                [format!("2 {hash}")],
            )
            .unwrap();
        assert!(store.verify_log().is_err());
        let mut replay = f.reader(Some("start"));
        assert!(matches!(replay.poll(10), Err(ReadError::Store(_))));
        let mut resumed = f.reader(Some(&before));
        assert!(matches!(resumed.poll(10), Err(ReadError::Store(_))));
        assert!(matches!(
            Reader::open(&f.path(), GRAPH, None),
            Err(ReadError::Store(_))
        ));
    }

    /// A store journaled by builds from before its log, then upgraded into
    /// the chain and migrated (friction item 85): its first entry is an
    /// `observe_main` whose report lists leaves in the order scopes had
    /// before 0.7 compared them by their lowercase form. `old_report` is
    /// that entry's report as an older build recorded it.
    fn migrated(old_report: Value) -> Fixture {
        let f = Fixture::new();
        let mut store = f.store();
        let observation = mdkg_coord::Observation {
            read_generation: 0,
            leaves: [
                (Scope::new(".mdkg/README.md"), "sha256:aa".to_string()),
                (Scope::new(".mdkg/config.json"), "sha256:bb".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let observed = store
            .execute(&Command::ObserveMain { observation })
            .unwrap();
        assert_eq!(
            serde_json::to_value(&observed).unwrap()["report"]["external"],
            json!([".mdkg/config.json", ".mdkg/README.md"]),
            "today's order is case-blind"
        );
        store.execute(&claim("a", "w/one", None)).unwrap();
        drop(store);
        let conn = Connection::open(f.path()).unwrap();
        conn.execute(
            "UPDATE journal SET outcome = ?1 WHERE seq = 1",
            [json!({"result": "observed", "report": old_report}).to_string()],
        )
        .unwrap();
        // Back to a store from before the log (schema "2").
        conn.execute_batch(
            "DROP TABLE log; DELETE FROM meta WHERE key IN ('log_head', 'last_checkpoint');
             UPDATE meta SET value = '2' WHERE key = 'schema';",
        )
        .unwrap();
        drop(conn);
        let mut store = f.store();
        store.execute(&claim("b", "w/two", None)).unwrap();
        store.send(&sent("after-genesis")).unwrap();
        // What `migrate` imports: a marker, then legacy records.
        let legacy = log::Imported {
            ts_ms: 1,
            actor: "mdkg".into(),
            kind: "legacy.event".into(),
            subject: vec![],
            body: serde_json::from_value(json!({"migrate_digest": "sha256:cc"})).unwrap(),
        };
        store.import_records("plan", &[legacy]).unwrap();
        store.execute(&claim("c", "w/three", None)).unwrap();
        f
    }

    fn kinds(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e.kind.as_str() {
                "mdkg.record.appended" => format!("record:{}", e.data["kind"].as_str().unwrap()),
                other => other.to_string(),
            })
            .collect()
    }

    #[test]
    fn a_migrated_store_replays_from_its_first_entry() {
        // The order a build from before 0.7 recorded: byte order.
        let f = migrated(json!({
            "committed": [], "conflicted": [],
            "external": [".mdkg/README.md", ".mdkg/config.json"],
        }));
        let events = f.events();
        assert_eq!(
            kinds(&events),
            [
                "record:log.genesis",
                "mdkg.lease.claimed",
                "mdkg.message.sent",
                "record:log.import",
                "record:legacy.event",
                "mdkg.lease.claimed",
            ]
        );
        assert_eq!(events[4].data["imported"], true);
        assert!(events.iter().all(|e| e.data.get("opaque").is_none()));
        // The lease taken before the log is part of the state replayed.
        assert_eq!(events[5].data["lease"]["id"], json!("lease-2"));
        // A resumed reader replays the same span to reach its cursor.
        let mut resumed = f.reader(Some(&events[0].cursor));
        assert_eq!(
            kinds(&resumed.poll(20).unwrap().events),
            kinds(&events)[1..]
        );
    }

    #[test]
    fn a_pre_log_journal_that_diverges_leaves_coordinator_records_plain() {
        // A difference beyond order: the state that span reaches can't be
        // trusted, so no lease state is derived from it, and mail still is.
        let f = migrated(json!({
            "committed": [], "conflicted": [], "external": [".mdkg/README.md"],
        }));
        let events = f.events();
        assert_eq!(
            kinds(&events),
            [
                "record:log.genesis",
                "record:lease.claim",
                "mdkg.message.sent",
                "record:log.import",
                "record:legacy.event",
                "record:lease.claim",
            ]
        );
        assert!(events[1].subjects.iter().any(|s| s.contains("/leases/")));
        assert_eq!(events[1].data["refused"], false);
        // The genesis says where replay stopped; every coordinator record
        // after it says it has no derived state; others carry no mark.
        assert_eq!(events[0].data["opaque"], true);
        assert_eq!(events[0].data["diverged_at"], 1);
        for i in [1, 5] {
            assert_eq!(events[i].data["opaque"], true, "{i}");
        }
        for i in [2, 3, 4] {
            assert!(events[i].data.get("opaque").is_none(), "{i}");
        }
        // A reader resuming past the genesis marks the same records: the
        // mark comes from replaying the span, not from streaming it.
        for at in [0, 2] {
            let mut resumed = f.reader(Some(&events[at].cursor));
            let rest = resumed.poll(20).unwrap().events;
            assert_eq!(kinds(&rest), kinds(&events)[at + 1..], "{at}");
            for (e, full) in rest.iter().zip(&events[at + 1..]) {
                assert_eq!(e.data, full.data, "{at}");
            }
        }
    }

    #[test]
    fn a_divergence_after_genesis_is_still_an_error() {
        let f = Fixture::new();
        let mut store = f.store();
        store.execute(&claim("a", "w/one", None)).unwrap();
        store.execute(&claim("b", "w/one", None)).unwrap();
        drop(store);
        // The second claim was refused; a journal that says it wasn't no
        // longer replays, and the record still holds the refusal.
        let conn = Connection::open(f.path()).unwrap();
        let lease: String = conn
            .query_row("SELECT outcome FROM journal WHERE seq = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute("UPDATE journal SET outcome = ?1 WHERE seq = 2", [lease])
            .unwrap();
        drop(conn);
        let mut r = f.reader(Some("start"));
        assert!(matches!(
            r.poll(20),
            Err(ReadError::Store(StoreError::ReplayDiverged(2)))
        ));
    }
}
