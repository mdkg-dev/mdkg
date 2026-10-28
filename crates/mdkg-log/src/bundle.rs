//! Anchors and work bundles for verifying a store's record chain independently.
//!
//! - An **anchor** (`.mdkg/anchors/<store-id>.json`, committed to git)
//!   names a chain's head hash at some seq: a small commitment that every
//!   later copy of the chain must still contain.
//! - A **bundle** (`export --work`) is the whole chain as JSON lines: a
//!   header, then one canonical record per line. Anyone can verify it
//!   without the store, and against an anchor.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::record::{GENESIS, Record, RecordError, parse_ts, valid_hash, verify_links};

pub const ANCHOR_FORMAT: &str = "mdkg-anchor";
pub const BUNDLE_FORMAT: &str = "mdkg-work-bundle";
/// The anchor and bundle format version.
pub const VERSION: u64 = 1;

/// Whether `id` is a store id: 32 lowercase hex digits, as stores mint
/// them (it names the anchor's file).
pub fn valid_store_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    pub format: String,
    pub v: u64,
    /// The store's id (its `store_id`).
    pub store: String,
    pub seq: u64,
    /// The hash of the record at `seq`.
    pub head: String,
    /// When the anchor was written (RFC 3339 UTC to the millisecond).
    pub ts: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleHeader {
    pub format: String,
    pub v: u64,
    pub store: String,
    /// How many records follow.
    pub count: u64,
    /// The hash of the last record.
    pub head: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleError {
    /// Not an anchor or bundle this build reads.
    Format(String),
    Record {
        line: usize,
        error: RecordError,
    },
    /// The chain doesn't match its header or the anchor.
    Mismatch(String),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BundleError::Format(why) | BundleError::Mismatch(why) => f.write_str(why),
            BundleError::Record { line, error } => write!(f, "line {line}: {error}"),
        }
    }
}

impl std::error::Error for BundleError {}

fn canonical(value: &impl Serialize) -> Result<String, BundleError> {
    let value = serde_json::to_value(value).map_err(|e| BundleError::Format(e.to_string()))?;
    mdkg_format::canonical::to_canonical_string(&value)
        .map_err(|e| BundleError::Format(e.to_string()))
}

/// Parses `text` as `T`, requiring it to be exactly `T`'s canonical form.
fn strict<T: Serialize + for<'de> Deserialize<'de>>(
    text: &str,
    what: &str,
) -> Result<T, BundleError> {
    let parsed: T = serde_json::from_str(text)
        .map_err(|e| BundleError::Format(format!("not a {what}: {e}")))?;
    if canonical(&parsed)? != text {
        return Err(BundleError::Format(format!(
            "the {what} is not in canonical form"
        )));
    }
    Ok(parsed)
}

impl Anchor {
    pub fn new(store: &str, seq: u64, head: &str, ts_ms: u64) -> Self {
        Anchor {
            format: ANCHOR_FORMAT.into(),
            v: VERSION,
            store: store.into(),
            seq,
            head: head.into(),
            ts: crate::record::format_ts(ts_ms),
        }
    }

    /// The file's text: canonical JSON and a newline.
    pub fn to_text(&self) -> Result<String, BundleError> {
        self.check()?;
        Ok(format!("{}\n", canonical(self)?))
    }

    pub fn parse(text: &str) -> Result<Self, BundleError> {
        let line = text.strip_suffix('\n').unwrap_or(text);
        let anchor: Anchor = strict(line, "anchor")?;
        anchor.check()?;
        Ok(anchor)
    }

    fn check(&self) -> Result<(), BundleError> {
        if self.format != ANCHOR_FORMAT || self.v != VERSION {
            return Err(BundleError::Format(format!(
                "unsupported anchor {}/{}",
                self.format, self.v
            )));
        }
        if !valid_store_id(&self.store)
            || self.seq == 0
            || !valid_hash(&self.head)
            || parse_ts(&self.ts).is_none()
        {
            return Err(BundleError::Format(
                "the anchor's fields are malformed".into(),
            ));
        }
        Ok(())
    }

    /// Checks that `chain` (records with their hashes, from seq 1) holds
    /// this anchor: the same store, and the anchored record unchanged.
    pub fn check_chain(&self, store: &str, chain: &[(Record, String)]) -> Result<(), BundleError> {
        if self.store != store {
            return Err(BundleError::Mismatch(format!(
                "the anchor is for store {}, not {store}",
                self.store
            )));
        }
        let at = usize::try_from(self.seq - 1)
            .ok()
            .and_then(|i| chain.get(i));
        match at {
            Some((r, h)) if r.seq == self.seq && *h == self.head => Ok(()),
            Some((_, h)) => Err(BundleError::Mismatch(format!(
                "the anchor names {} at seq {}, but the chain has {h}: the chain was rewritten or \
                 forked after the anchor (a restored store, or a tampered one)",
                self.head, self.seq
            ))),
            None => Err(BundleError::Mismatch(format!(
                "the anchor is at seq {}, past the chain's end ({})",
                self.seq,
                chain.len()
            ))),
        }
    }
}

/// The store id a chain's genesis record names.
pub fn genesis_store(chain: &[(Record, String)]) -> Option<&str> {
    chain
        .first()
        .filter(|(r, _)| r.seq == 1 && r.kind == GENESIS)
        .and_then(|(r, _)| r.body.get("store"))
        .and_then(Value::as_str)
}

/// Writes a bundle of `chain` (canonical record texts in seq order, from
/// seq 1) for `store`.
pub fn write_bundle(store: &str, texts: &[String]) -> Result<String, BundleError> {
    let chain = parse_texts(texts.iter().map(String::as_str), 1)?;
    check_chain(store, &chain)?;
    let head = chain.last().map(|(_, h)| h.clone()).unwrap_or_default();
    let header = BundleHeader {
        format: BUNDLE_FORMAT.into(),
        v: VERSION,
        store: store.into(),
        count: chain.len() as u64,
        head,
    };
    let mut out = canonical(&header)?;
    out.push('\n');
    for text in texts {
        out.push_str(text);
        out.push('\n');
    }
    Ok(out)
}

fn parse_texts<'a>(
    texts: impl Iterator<Item = &'a str>,
    first_line: usize,
) -> Result<Vec<(Record, String)>, BundleError> {
    texts
        .enumerate()
        .map(|(i, t)| {
            Record::parse(t).map_err(|error| BundleError::Record {
                line: first_line + i,
                error,
            })
        })
        .collect()
}

/// A whole chain for `store`: from a genesis naming it, linked throughout.
pub fn check_chain(store: &str, chain: &[(Record, String)]) -> Result<(), BundleError> {
    if chain.first().is_none_or(|(r, _)| r.seq != 1) {
        return Err(BundleError::Mismatch(
            "the chain doesn't start at seq 1".into(),
        ));
    }
    verify_links(chain).map_err(|error| BundleError::Record { line: 0, error })?;
    match genesis_store(chain) {
        Some(s) if s == store => Ok(()),
        other => Err(BundleError::Mismatch(format!(
            "the chain's genesis names store {other:?}, not {store}"
        ))),
    }
}

/// What a verified bundle holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub store: String,
    pub count: u64,
    pub head: String,
    /// The seq the anchor checked, when one was given.
    pub anchored: Option<u64>,
    pub chain: Vec<(Record, String)>,
}

/// Verifies a bundle with no store involved, and against `anchor` if
/// given: every line canonical, the chain whole from its genesis, and the
/// header's count and head those of the chain.
pub fn verify_bundle(text: &str, anchor: Option<&Anchor>) -> Result<Verified, BundleError> {
    let body = text
        .strip_suffix('\n')
        .ok_or_else(|| BundleError::Format("a bundle ends with a newline".into()))?;
    let mut lines = body.split('\n');
    let header: BundleHeader = strict(lines.next().unwrap_or_default(), "bundle header")?;
    if header.format != BUNDLE_FORMAT || header.v != VERSION {
        return Err(BundleError::Format(format!(
            "unsupported bundle {}/{}",
            header.format, header.v
        )));
    }
    if !valid_store_id(&header.store) {
        return Err(BundleError::Format(format!(
            "the bundle names store {:?}, which isn't a store id",
            header.store
        )));
    }
    let chain = parse_texts(lines, 2)?;
    check_chain(&header.store, &chain)?;
    let head = chain.last().map(|(_, h)| h.as_str()).unwrap_or_default();
    if header.count != chain.len() as u64 || header.head != head {
        return Err(BundleError::Mismatch(format!(
            "the header says {} records ending {}, but the bundle holds {} ending {head}",
            header.count,
            header.head,
            chain.len()
        )));
    }
    if let Some(anchor) = anchor {
        anchor.check_chain(&header.store, &chain)?;
    }
    Ok(Verified {
        store: header.store,
        count: header.count,
        head: header.head,
        anchored: anchor.map(|a| a.seq),
        chain,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;

    const STORE: &str = "0123456789abcdef0123456789abcdef";

    fn texts(n: u64) -> Vec<String> {
        let mut out: Vec<(Record, String, String)> = Vec::new();
        for i in 0..n {
            let mut body = Map::new();
            let kind = if i == 0 {
                body.insert("store".into(), STORE.into());
                body.insert("journal_seq".into(), 0.into());
                GENESIS
            } else {
                body.insert("i".into(), i.into());
                "lease.claim"
            };
            let after = out.last().map(|(r, h, _)| (r.seq, h.as_str()));
            let r = Record::next(after, 1_000 + i, "a", kind, vec![], body);
            let (t, h) = r.seal().unwrap();
            out.push((r, h, t));
        }
        out.into_iter().map(|(_, _, t)| t).collect()
    }

    #[test]
    fn bundles_verify_and_anchor() {
        let t = texts(4);
        let bundle = write_bundle(STORE, &t).unwrap();
        let v = verify_bundle(&bundle, None).unwrap();
        assert_eq!((v.count, v.store.as_str()), (4, STORE));
        let anchor = Anchor::new(STORE, 2, &v.chain[1].1, 5_000);
        let parsed = Anchor::parse(&anchor.to_text().unwrap()).unwrap();
        assert_eq!(parsed, anchor);
        assert_eq!(
            verify_bundle(&bundle, Some(&anchor)).unwrap().anchored,
            Some(2)
        );
    }

    #[test]
    fn tampering_is_found() {
        let t = texts(3);
        let bundle = write_bundle(STORE, &t).unwrap();
        // A record changed in place.
        let changed = bundle.replace("\"i\":2", "\"i\":9");
        assert!(verify_bundle(&changed, None).is_err());
        // A record dropped from the end: the header no longer matches.
        let lines: Vec<&str> = bundle.lines().collect();
        let short = format!("{}\n", lines[..3].join("\n"));
        assert!(matches!(
            verify_bundle(&short, None),
            Err(BundleError::Mismatch(_))
        ));
        // Truncated and re-headed: an anchor past the end catches it.
        let short_bundle = write_bundle(STORE, &t[..2]).unwrap();
        let full = verify_bundle(&bundle, None).unwrap();
        let anchor = Anchor::new(STORE, 3, &full.head, 1);
        assert!(verify_bundle(&short_bundle, Some(&anchor)).is_err());
        // A forked chain: same length, different record 3.
        let mut forked = t[..2].to_vec();
        let (r2, h2) = Record::parse(&t[1]).unwrap();
        let mut body = Map::new();
        body.insert("fork".into(), true.into());
        let r3 = Record::next(Some((r2.seq, &h2)), 7, "b", "lease.claim", vec![], body);
        forked.push(r3.seal().unwrap().0);
        let forked = write_bundle(STORE, &forked).unwrap();
        assert!(verify_bundle(&forked, None).is_ok());
        let err = verify_bundle(&forked, Some(&anchor)).unwrap_err();
        assert!(err.to_string().contains("forked"), "{err}");
        // Another store's anchor, and another store's chain.
        let other = Anchor::new(&"f".repeat(32), 1, &full.chain[0].1, 1);
        assert!(verify_bundle(&bundle, Some(&other)).is_err());
        assert!(write_bundle(&"f".repeat(32), &t).is_err());
        assert!(
            Anchor::new("../x", 1, &full.chain[0].1, 1)
                .to_text()
                .is_err()
        );
    }

    #[test]
    fn strict_forms() {
        let t = texts(2);
        let bundle = write_bundle(STORE, &t).unwrap();
        assert!(verify_bundle(bundle.trim_end(), None).is_err(), "newline");
        assert!(
            verify_bundle(&format!("{bundle}\n"), None).is_err(),
            "blank line"
        );
        let spaced = bundle.replacen("{\"count\"", "{ \"count\"", 1);
        assert!(verify_bundle(&spaced, None).is_err());
        let a = Anchor::new(STORE, 1, &Record::parse(&t[0]).unwrap().1, 1);
        let text = a.to_text().unwrap();
        assert!(Anchor::parse(&text.replace("\"v\":1", "\"v\":2")).is_err());
        assert!(Anchor::parse(&text.replacen('{', "{\"x\":0,", 1)).is_err());
    }
}
