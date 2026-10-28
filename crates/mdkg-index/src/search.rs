//! Full-text search: FTS5 over ids, titles, fields and bodies, ranked by
//! BM25.
//!
//! A query is words; every word must match, as the start of a word in the
//! text (`pars` finds "parser"). Words are always quoted for FTS5, so no
//! typed text is read as query syntax.

use rusqlite::params;

use crate::store::{Db, IndexError};

/// BM25 column weights: id, title, fields, body. A word in a title counts
/// for ten in a body.
const WEIGHTS: (f64, f64, f64, f64) = (4.0, 10.0, 3.0, 1.0);

/// Words a snippet shows around the matches.
const SNIPPET_TOKENS: i64 = 16;

/// What marks a match in a snippet (markdown bold).
pub const MARK: &str = "**";

/// One match, best first.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub qid: String,
    /// BM25 relevance; higher is better.
    pub score: f64,
}

/// A score as output shows it: six significant digits. BM25 gives a word
/// found in most nodes an idf of 1e-6, so a fixed number of decimals would
/// show every such hit as 0 (RC1 F6); this keeps their order visible.
pub fn shown_score(score: f64) -> f64 {
    if score == 0.0 || !score.is_finite() {
        return score;
    }
    let scale = 10f64.powi(5 - score.abs().log10().floor() as i32);
    (score * scale).round() / scale
}

/// The FTS5 expression for a query: each word that has a letter or digit,
/// quoted and matched as a prefix, all of them required. `None` when no
/// word is left.
pub fn fts_query(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(mdkg_format::js::is_js_whitespace)
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
        .collect();
    (!words.is_empty()).then(|| words.join(" AND "))
}

/// Every node matching `text`, best first (ties by qid).
pub fn search(db: &Db, text: &str) -> Result<Vec<Hit>, IndexError> {
    let Some(expr) = fts_query(text) else {
        return Ok(Vec::new());
    };
    let (a, b, c, d) = WEIGHTS;
    let mut stmt = db.conn().prepare_cached(
        "SELECT n.qid, bm25(fts, ?2, ?3, ?4, ?5) AS rank \
         FROM fts JOIN nodes n ON n.rowid = fts.rowid \
         WHERE fts MATCH ?1 ORDER BY rank, n.qid",
    )?;
    let hits = stmt
        .query_map(params![expr, a, b, c, d], |r| {
            Ok(Hit {
                qid: r.get(0)?,
                score: -r.get::<_, f64>(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(hits)
}

/// The best passage of one matching node's body, matches marked with
/// [`MARK`]. `None` when the body has no match (the words matched its id,
/// title or fields).
pub fn snippet(db: &Db, text: &str, qid: &str) -> Result<Option<String>, IndexError> {
    let Some(expr) = fts_query(text) else {
        return Ok(None);
    };
    let mut stmt = db.conn().prepare_cached(
        "SELECT snippet(fts, 3, ?3, ?3, '…', ?4) FROM fts \
         WHERE fts MATCH ?1 AND fts.rowid = (SELECT rowid FROM nodes WHERE qid = ?2)",
    )?;
    let mut rows = stmt.query(params![expr, qid, MARK, SNIPPET_TOKENS])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get::<_, String>(0)?)
            .filter(|s| s.contains(MARK))
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")),
        None => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_quote_every_word() {
        assert_eq!(fts_query("parser"), Some("\"parser\"*".into()));
        assert_eq!(
            fts_query("  Parser  format "),
            Some("\"Parser\"* AND \"format\"*".into())
        );
        assert_eq!(
            fts_query("a\"b OR NEAR(x)"),
            Some("\"a\"\"b\"* AND \"OR\"* AND \"NEAR(x)\"*".into())
        );
        assert_eq!(fts_query(" - * \" "), None);
    }

    #[test]
    fn scores_keep_six_significant_digits() {
        assert_eq!(shown_score(5.848_123_9), 5.848_12);
        assert_eq!(shown_score(0.000_001_234_567_8), 0.000_001_234_57);
        assert!(shown_score(0.000_002_1) > shown_score(0.000_001_9));
        assert_eq!(shown_score(0.0), 0.0);
    }
}
