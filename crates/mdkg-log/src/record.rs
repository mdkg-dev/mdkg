//! Work-state records: one hash-linked
//! chain per coordinator store.
//!
//! A record is the RFC 8785 canonical JSON object
//! `{v, seq, prev, ts, actor, kind, subject, body}`, and its hash is the
//! sha256 of those bytes. `prev` names the previous record's hash, so the
//! head hash commits to the whole chain: an anchor in git that names it
//! lets anyone check a store or an exported bundle against it.
//!
//! This module is sans-IO: the store writes records inside its own
//! transactions, and bundles and anchors are text in and text out.

use mdkg_format::Digest;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The record format version this build writes and reads.
pub const RECORD_VERSION: u64 = 1;

/// The kind of every chain's first record.
pub const GENESIS: &str = "log.genesis";

/// A node's subject entry. Records name nodes by their 0.7 id (format
/// section 9), as in-graph refs write it; an id that isn't one
/// (a 0.6.0 id, before `migrate`) goes as `legacy:<id>`, so a bare
/// subject is always a 0.7 id.
pub fn node_ref(id: &str) -> String {
    match mdkg_format::id::NodeId::parse(id) {
        Ok(node) => node.to_string(),
        Err(_) => format!("legacy:{id}"),
    }
}

/// One record. Field order here is irrelevant: the canonical form sorts
/// keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub v: u64,
    pub seq: u64,
    pub prev: Option<String>,
    /// RFC 3339 UTC to the millisecond (`YYYY-MM-DDTHH:MM:SS.sssZ`).
    pub ts: String,
    pub actor: String,
    pub kind: String,
    pub subject: Vec<String>,
    pub body: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// Not JSON, not a record, or not in canonical form.
    Malformed(String),
    /// A version this build doesn't read.
    Version(u64),
    /// The chain breaks at this seq.
    Chain { seq: u64, why: String },
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::Malformed(why) => write!(f, "malformed record: {why}"),
            RecordError::Version(v) => write!(
                f,
                "record version {v} is newer than this build reads ({RECORD_VERSION})"
            ),
            RecordError::Chain { seq, why } => write!(f, "the chain breaks at seq {seq}: {why}"),
        }
    }
}

impl std::error::Error for RecordError {}

fn malformed(e: impl std::fmt::Display) -> RecordError {
    RecordError::Malformed(e.to_string())
}

/// Whether `kind` is `<domain>.<verb>`: two or more dot-separated segments
/// of lowercase ASCII letters, digits and `_`.
pub fn valid_kind(kind: &str) -> bool {
    let parts: Vec<&str> = kind.split('.').collect();
    parts.len() >= 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// Whether `hash` is `sha256:` and 64 lowercase hex digits.
pub fn valid_hash(hash: &str) -> bool {
    hash.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

impl Record {
    /// The record after the one at `(seq, hash)`, or the first of a chain.
    pub fn next(
        after: Option<(u64, &str)>,
        ts_ms: u64,
        actor: &str,
        kind: &str,
        subject: Vec<String>,
        body: Map<String, Value>,
    ) -> Record {
        Record {
            v: RECORD_VERSION,
            seq: after.map_or(1, |(seq, _)| seq + 1),
            prev: after.map(|(_, hash)| hash.to_string()),
            ts: format_ts(ts_ms),
            actor: actor.to_string(),
            kind: kind.to_string(),
            subject,
            body,
        }
    }

    /// The canonical text, after checking the record is well formed.
    pub fn to_canonical(&self) -> Result<String, RecordError> {
        self.check()?;
        let value = serde_json::to_value(self).map_err(malformed)?;
        mdkg_format::canonical::to_canonical_string(&value).map_err(malformed)
    }

    /// The canonical text and its hash (`sha256:<hex>`).
    pub fn seal(&self) -> Result<(String, String), RecordError> {
        let text = self.to_canonical()?;
        let hash = Digest::of_bytes(text.as_bytes()).to_string();
        Ok((text, hash))
    }

    /// Parses one stored record strictly: the text must be exactly the
    /// canonical form of what it parses to. Returns the record and its
    /// hash.
    pub fn parse(text: &str) -> Result<(Record, String), RecordError> {
        let value: Value = serde_json::from_str(text).map_err(malformed)?;
        if let Some(v) = value.get("v").and_then(Value::as_u64)
            && v > RECORD_VERSION
        {
            return Err(RecordError::Version(v));
        }
        let record: Record = serde_json::from_value(value).map_err(malformed)?;
        let (canonical, hash) = record.seal()?;
        if canonical != text {
            return Err(RecordError::Malformed(format!(
                "record {} is not in canonical form",
                record.seq
            )));
        }
        Ok((record, hash))
    }

    fn check(&self) -> Result<(), RecordError> {
        let bad = |why: String| Err(RecordError::Malformed(why));
        if self.v != RECORD_VERSION {
            return if self.v > RECORD_VERSION {
                Err(RecordError::Version(self.v))
            } else {
                bad(format!("version {}", self.v))
            };
        }
        if self.seq == 0 {
            return bad("seq 0".into());
        }
        match (&self.prev, self.seq) {
            (None, 1) => {}
            (Some(p), s) if s > 1 && valid_hash(p) => {}
            (p, s) => return bad(format!("seq {s} with prev {p:?}")),
        }
        if parse_ts(&self.ts).is_none() {
            return bad(format!("seq {}: timestamp {:?}", self.seq, self.ts));
        }
        if self.actor.is_empty() {
            return bad(format!("seq {}: empty actor", self.seq));
        }
        if !valid_kind(&self.kind) {
            return bad(format!("seq {}: kind {:?}", self.seq, self.kind));
        }
        if self.subject.iter().any(String::is_empty) {
            return bad(format!("seq {}: empty subject", self.seq));
        }
        Ok(())
    }
}

/// Checks that `records`, with the hashes `Record::parse` or `seal` gave
/// them, form one chain from its first record: seqs count up by one and each `prev` is the hash before.
/// A chain starting at seq 1 must open with `log.genesis`.
pub fn verify_links(records: &[(Record, String)]) -> Result<(), RecordError> {
    let mut before: Option<(u64, &str)> = None;
    for (record, own) in records {
        if let Some((seq, hash)) = before {
            if record.seq != seq + 1 {
                return Err(RecordError::Chain {
                    seq: record.seq,
                    why: format!("follows seq {seq}"),
                });
            }
            if record.prev.as_deref() != Some(hash) {
                return Err(RecordError::Chain {
                    seq: record.seq,
                    why: format!("prev {:?} isn't {hash}", record.prev),
                });
            }
        } else if record.seq == 1 && record.kind != GENESIS {
            return Err(RecordError::Chain {
                seq: 1,
                why: format!("the first record is {}, not {GENESIS}", record.kind),
            });
        }
        before = Some((record.seq, own.as_str()));
    }
    Ok(())
}

/// Milliseconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SS.sssZ`.
pub fn format_ts(ms: u64) -> String {
    let secs = ms / 1000;
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX / 2);
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

/// The milliseconds a `format_ts` string names, if it is exactly one.
pub fn parse_ts(ts: &str) -> Option<u64> {
    let b = ts.as_bytes();
    if b.len() != 24
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'.'
        || b[23] != b'Z'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u64> {
        let s = ts.get(r)?;
        s.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| s.parse().ok())
            .flatten()
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, m, s, frac) = (num(11..13)?, num(14..16)?, num(17..19)?, num(20..23)?);
    if !(1..=12).contains(&month) || day == 0 || h > 23 || m > 59 || s > 59 || year < 1970 {
        return None;
    }
    let days = days_from_civil(i64::try_from(year).ok()?, month, day);
    let ms = u64::try_from(days).ok()? * 86_400_000 + (h * 3600 + m * 60 + s) * 1000 + frac;
    // Rejects days past the month's end (Feb 30 reads back as Mar 2).
    (format_ts(ms) == ts).then_some(ms)
}

// Howard Hinnant's civil-date algorithms.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        u64::try_from(month).unwrap_or_default(),
        u64::try_from(day).unwrap_or_default(),
    )
}

fn days_from_civil(year: i64, month: u64, day: u64) -> i64 {
    let (month, day) = (
        i64::try_from(month).unwrap_or_default(),
        i64::try_from(day).unwrap_or_default(),
    );
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(n: u64) -> Vec<(Record, String)> {
        let mut out: Vec<(Record, String)> = Vec::new();
        for i in 0..n {
            let kind = if i == 0 { GENESIS } else { "lease.claim" };
            let after = out.last().map(|(r, h)| (r.seq, h.as_str()));
            let mut body = Map::new();
            body.insert("i".into(), Value::from(i));
            let r = Record::next(
                after,
                1_000 + i,
                "a",
                kind,
                vec![node_ref("task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b")],
                body,
            );
            let (_, h) = r.seal().unwrap();
            out.push((r, h));
        }
        out
    }

    #[test]
    fn subjects_name_0_7_ids_bare_and_old_ids_as_legacy() {
        let id = "task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b";
        assert_eq!(node_ref(id), id);
        assert_eq!(node_ref("task-39"), "legacy:task-39");
        assert_eq!(node_ref("root:task-39"), "legacy:root:task-39");
    }

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(format_ts(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_ts(951_782_400_123), "2000-02-29T00:00:00.123Z");
        assert_eq!(format_ts(1_792_303_505_007), "2026-10-18T06:05:05.007Z");
        for ms in [0, 1, 951_782_400_123, 1_792_303_505_007, 4_102_444_799_999] {
            assert_eq!(parse_ts(&format_ts(ms)), Some(ms));
        }
        for bad in [
            "2026-02-30T00:00:00.000Z",
            "2026-10-18T06:05:05Z",
            "2026-10-18T06:05:05.007+00:00",
            "2026-10-18 06:05:05.007Z",
            "2026-13-01T00:00:00.000Z",
            "+026-10-18T06:05:05.007Z",
        ] {
            assert_eq!(parse_ts(bad), None, "{bad}");
        }
    }

    #[test]
    fn records_seal_parse_and_link() {
        let c = chain(3);
        verify_links(&c).unwrap();
        for (r, h) in &c {
            let (text, hash) = r.seal().unwrap();
            assert_eq!(&hash, h);
            assert!(valid_hash(&hash));
            assert_eq!(Record::parse(&text).unwrap(), (r.clone(), hash));
        }
        assert_eq!(c[1].0.prev.as_deref(), Some(c[0].1.as_str()));
        assert!(c[0].0.prev.is_none());
    }

    #[test]
    fn strict_parsing() {
        let (r, _) = &chain(1)[0];
        let text = r.to_canonical().unwrap();
        // Whitespace, an extra key, a newer version.
        let spaced = text.replacen(',', ", ", 1);
        assert!(matches!(
            Record::parse(&spaced),
            Err(RecordError::Malformed(_))
        ));
        let extra = text.replacen('{', "{\"x\":1,", 1);
        assert!(Record::parse(&extra).is_err());
        let newer = text.replace("\"v\":1", "\"v\":2");
        assert_eq!(Record::parse(&newer), Err(RecordError::Version(2)));
        // A float in the body can't be canonical.
        let float = text.replace("\"i\":0", "\"i\":0.5");
        assert!(Record::parse(&float).is_err());
    }

    #[test]
    fn malformed_records_are_refused() {
        let (r, _) = &chain(1)[0];
        for f in [
            |r: &mut Record| r.seq = 0,
            |r: &mut Record| r.prev = Some("sha256:00".into()),
            |r: &mut Record| r.ts = "yesterday".into(),
            |r: &mut Record| r.actor = String::new(),
            |r: &mut Record| r.kind = "Lease".into(),
            |r: &mut Record| r.subject = vec![String::new()],
            |r: &mut Record| r.v = 0,
        ] {
            let mut bad = r.clone();
            f(&mut bad);
            assert!(bad.seal().is_err(), "{bad:?}");
        }
        let mut second = r.clone();
        second.seq = 2;
        assert!(second.seal().is_err(), "seq 2 needs a prev");
    }

    #[test]
    fn broken_chains_are_found() {
        let c = chain(4);
        let mut gap = c.clone();
        gap.remove(2);
        assert!(matches!(
            verify_links(&gap),
            Err(RecordError::Chain { seq: 4, .. })
        ));
        let mut swapped = c.clone();
        swapped[2].0.prev = Some(c[0].1.clone());
        swapped[2].1 = swapped[2].0.seal().unwrap().1;
        assert!(matches!(
            verify_links(&swapped),
            Err(RecordError::Chain { seq: 3, .. })
        ));
        let mut no_genesis = c.clone();
        no_genesis[0].0.kind = "lease.claim".into();
        assert!(verify_links(&no_genesis).is_err());
        // A suffix verifies on its own.
        verify_links(&c[2..]).unwrap();
    }
}
