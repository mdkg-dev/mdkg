//! The log records a migration writes to the coordination plane: legacy
//! events, checkpoints, check results and review records.
//!
//! A record's digest is the sha256 of its canonical JSON, so the plan, and
//! the summary lines left in nodes, can cite a record before it's written.

use mdkg_format::Digest;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// What a record holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RecordKind {
    /// A line of a 0.6.0 `events.jsonl`.
    #[serde(rename = "legacy.event")]
    LegacyEvent,
    /// A 0.6.0 checkpoint node.
    #[serde(rename = "legacy.checkpoint")]
    LegacyCheckpoint,
    /// A finished test or proof node.
    #[serde(rename = "check.result")]
    CheckResult,
    /// A progress or review-ledger section of a node.
    #[serde(rename = "review.record")]
    ReviewRecord,
}

/// One record for the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Record {
    pub kind: RecordKind,
    /// When it happened, RFC 3339 UTC to the millisecond (a 0.6.0 date is
    /// its day's start).
    pub ts: String,
    pub actor: String,
    /// The nodes it's about, by their new ids.
    pub subject: Vec<String>,
    pub body: Value,
    /// sha256 of the canonical JSON of the fields above.
    pub digest: String,
}

/// The actor of records whose 0.6.0 source names none.
pub const LEGACY_ACTOR: &str = "mdkg-0.6";

impl Record {
    pub fn new(
        kind: RecordKind,
        ts: impl Into<String>,
        actor: impl Into<String>,
        subject: Vec<String>,
        body: Value,
    ) -> Self {
        let (ts, actor) = (normalize_ts(&ts.into()), actor_name(&actor.into()));
        let content = json!({
            "kind": kind,
            "ts": ts,
            "actor": actor,
            "subject": subject,
            "body": body,
        });
        // A body can't hold floats or unsafe integers in canonical form;
        // such a body is kept as its JSON text instead.
        let (body, digest) = match Digest::of_record(&content) {
            Ok(d) => (body, d),
            Err(_) => {
                let text = Value::String(body.to_string());
                let wrapped = json!({ "noncanonical_json": text });
                let content = json!({
                    "kind": kind, "ts": ts, "actor": actor,
                    "subject": subject, "body": wrapped,
                });
                let d = Digest::of_record(&content)
                    .unwrap_or_else(|_| Digest::of_bytes(content.to_string().as_bytes()));
                (wrapped, d)
            }
        };
        Self {
            kind,
            ts,
            actor,
            subject,
            body,
            digest: digest.to_string(),
        }
    }
}

/// An actor name as the log takes it: lowercase `a-z`, `0-9`, `.`, `_`,
/// `-` and `:`, at most 120 characters; anything else becomes `-`, and an
/// empty name is the legacy actor. The source keeps the name as written.
pub fn actor_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | ':') {
                c
            } else {
                '-'
            }
        })
        .take(120)
        .collect();
    if cleaned.trim_matches('-').is_empty() {
        LEGACY_ACTOR.to_string()
    } else {
        cleaned
    }
}

/// Unix milliseconds of a timestamp in the form [`normalize_ts`] writes,
/// as lane 2's store import takes them.
pub fn ts_ms(ts: &str) -> u64 {
    let num =
        |r: std::ops::Range<usize>| ts.get(r).and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
    let day = super::mint::date_ms(ts.get(..10).unwrap_or("")).unwrap_or(0);
    day + num(11..13) * 3_600_000 + num(14..16) * 60_000 + num(17..19) * 1000 + num(20..23)
}

/// A timestamp as the log writes it, `YYYY-MM-DDTHH:MM:SS.sssZ`: a date
/// gets its day's start, a time without milliseconds gets `.000`, and
/// anything else becomes the epoch (the source keeps the original).
pub fn normalize_ts(ts: &str) -> String {
    const EPOCH: &str = "1970-01-01T00:00:00.000Z";
    let b = ts.as_bytes();
    let digits = |r: std::ops::Range<usize>| {
        r.into_iter()
            .all(|i| b.get(i).is_some_and(u8::is_ascii_digit))
    };
    let date_ok = b.len() >= 10
        && digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && super::mint::date_ms(&ts[..10]).is_some();
    if !date_ok {
        return EPOCH.into();
    }
    if b.len() == 10 {
        return format!("{ts}T00:00:00.000Z");
    }
    let time_ok = b.len() >= 20
        && b[10] == b'T'
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19);
    if !time_ok {
        return EPOCH.into();
    }
    match &ts[19..] {
        "Z" => format!("{}.000Z", &ts[..19]),
        rest if rest.len() == 5
            && rest.starts_with('.')
            && digits(20..23)
            && rest.ends_with('Z') =>
        {
            ts.to_string()
        }
        _ => EPOCH.into(),
    }
}

/// Reads one `events.jsonl` file into legacy event records, in file order.
/// A line that isn't a JSON object is kept whole, as text. `map` turns a
/// 0.6.0 ref into its new id. Returns the records and the 1-based numbers
/// of lines that weren't objects.
pub fn import_events(
    path: &str,
    content: &str,
    mut map: impl FnMut(&str) -> Option<String>,
) -> (Vec<Record>, Vec<usize>) {
    let mut records = Vec::new();
    let mut unparsed = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let n = i + 1;
        let parsed: Option<serde_json::Map<String, Value>> = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            });
        let Some(event) = parsed else {
            unparsed.push(n);
            let body = json!({ "source": path, "line": n, "text": line });
            records.push(Record::new(
                RecordKind::LegacyEvent,
                "",
                LEGACY_ACTOR,
                Vec::new(),
                body,
            ));
            continue;
        };
        let text = |k: &str| event.get(k).and_then(Value::as_str).map(str::to_string);
        let refs: Vec<String> = event
            .get("refs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let mapped: Vec<Value> = refs
            .iter()
            .map(|r| map(r).map_or(Value::Null, Value::String))
            .collect();
        // The nodes it names: by new id, or as `legacy:<id>` (lane 2's typed
        // ref) where this graph has no node by that id now.
        let mut subject: Vec<String> = refs
            .iter()
            .zip(&mapped)
            .map(|(r, m)| {
                m.as_str()
                    .map_or_else(|| format!("legacy:{r}"), str::to_string)
            })
            .collect();
        subject.sort();
        subject.dedup();
        let body = json!({
            "source": path,
            "line": n,
            "event": Value::Object(event.clone()),
            "refs": mapped,
        });
        records.push(Record::new(
            RecordKind::LegacyEvent,
            text("ts").unwrap_or_default(),
            text("agent").unwrap_or_else(|| LEGACY_ACTOR.to_string()),
            subject,
            body,
        ));
    }
    (records, unparsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actors_and_milliseconds() {
        assert_eq!(actor_name("mdkg-cli"), "mdkg-cli");
        assert_eq!(actor_name("Ada Lovelace"), "ada-lovelace");
        assert_eq!(actor_name("  "), LEGACY_ACTOR);
        assert_eq!(actor_name("é"), LEGACY_ACTOR);
        assert_eq!(ts_ms("1970-01-01T00:00:01.002Z"), 1002);
        assert_eq!(ts_ms("2026-10-24T00:00:00.019Z"), 1_792_800_000_019);
        assert_eq!(
            ts_ms(&normalize_ts("2026-10-24T01:02:03Z")),
            1_792_800_000_000 + 3_723_000
        );
    }

    #[test]
    fn timestamps() {
        assert_eq!(normalize_ts("2026-10-24"), "2026-10-24T00:00:00.000Z");
        assert_eq!(
            normalize_ts("2026-10-24T01:02:03Z"),
            "2026-10-24T01:02:03.000Z"
        );
        assert_eq!(
            normalize_ts("2026-10-24T01:02:03.456Z"),
            "2026-10-24T01:02:03.456Z"
        );
        assert_eq!(
            normalize_ts("2026-10-24T01:02:03+02:00"),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(normalize_ts("2026-02-30"), "1970-01-01T00:00:00.000Z");
        assert_eq!(normalize_ts(""), "1970-01-01T00:00:00.000Z");
        assert_eq!(normalize_ts("é"), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn events_import_whole() {
        let content = concat!(
            r#"{"ts":"2026-05-15T01:17:46.639Z","agent":"mdkg-cli","kind":"NODE_CREATED","refs":["task-1","gone-9"]}"#,
            "\n\n",
            "not json\n",
            r#"{"ts":"2026-05-15T01:18:00.000Z","kind":"X","n":1.5}"#,
            "\n",
        );
        let (records, unparsed) = import_events(".mdkg/work/events/events.jsonl", content, |r| {
            (r == "task-1").then(|| "task-new".to_string())
        });
        assert_eq!(records.len(), 3);
        assert_eq!(unparsed, vec![3]);
        let first = &records[0];
        assert_eq!(first.actor, "mdkg-cli");
        assert_eq!(first.ts, "2026-05-15T01:17:46.639Z");
        assert_eq!(first.subject, ["legacy:gone-9", "task-new"]);
        assert_eq!(first.body["refs"], json!(["task-new", null]));
        assert_eq!(first.body["event"]["kind"], "NODE_CREATED");
        assert_eq!(records[1].body["text"], "not json");
        assert_eq!(records[2].actor, LEGACY_ACTOR);
        assert_eq!(records[1].ts, "1970-01-01T00:00:00.000Z");
        // A float can't be canonical: the body is kept as text.
        assert!(records[2].body["noncanonical_json"].is_string());
        // Digests are stable and distinct.
        let again = import_events(".mdkg/work/events/events.jsonl", content, |r| {
            (r == "task-1").then(|| "task-new".to_string())
        })
        .0;
        assert_eq!(records, again);
        assert_ne!(records[0].digest, records[1].digest);
        assert!(records[0].digest.starts_with("sha256:"));
    }
}
