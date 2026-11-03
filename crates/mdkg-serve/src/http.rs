//! The parsed request and the response, as `mdkg-serve` sees them: the
//! types are `mdkg-wire`'s codec's (L1), which does HTTP/1.1 framing and
//! TLS behind [`Codec`](mdkg_wire::codec::Codec); everything after
//! parsing is here, through [`HeadExt`].

use mdkg_format::Code;
use mdkg_graph::ErrorKind;
pub use mdkg_wire::codec::{RequestHead, Response};

use crate::refusal::Refusal;

/// What serve reads from a request's head (S21), before any body byte:
/// header values, the query, the resume cursor and the body length.
pub trait HeadExt {
    /// Every value of the header `name` (lowercase).
    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a;

    /// The header's one value: `Err(())` when it appears more than once.
    /// Signed and routing headers are singletons; a repeat is refused
    /// rather than combined, so nothing reads one copy and verifies
    /// another.
    #[allow(clippy::result_unit_err)]
    fn one<'a>(&'a self, name: &'a str) -> Result<Option<&'a str>, ()> {
        let mut values = self.all(name);
        let first = values.next();
        if values.next().is_some() {
            return Err(());
        }
        Ok(first)
    }

    fn has(&self, name: &str) -> bool {
        self.all(name).next().is_some()
    }

    fn query_pairs(&self) -> Result<Vec<(String, String)>, Refusal>;

    fn stream_after(&self) -> Result<Option<String>, Refusal>;

    fn body_length(&self) -> Result<Option<u64>, Length>;
}

impl HeadExt for RequestHead {
    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The query's pairs, names and values percent-decoded, with `+` as a
    /// space, as form encoders send them. A pair without `=` has an empty
    /// value; empty pairs are skipped. A malformed escape or a result that
    /// isn't UTF-8 refuses the whole query. Read only after the signature,
    /// which covers the query as sent (`@query`), has verified.
    fn query_pairs(&self) -> Result<Vec<(String, String)>, Refusal> {
        let Some(query) = self.query.as_deref() else {
            return Ok(Vec::new());
        };
        query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                Ok((decode(name)?, decode(value)?))
            })
            .collect()
    }

    /// Where a stream resumes: only the signed `after` query parameter,
    /// decoded. `Ok(None)` when it is absent, and each route then keeps its
    /// own start: `/v1/events` at the head, a turn's output from its
    /// retained journal's start, `msg wait` with the unacked backlog
    /// (#7213). A repeated `after`, or a malformed query, is refused rather
    /// than read as absent, which would skip events. An empty `after` is
    /// returned as it is, for the route's cursor reader to refuse:
    /// `api.bad_cursor` with the head on `/v1/events` and a turn's output
    /// (Terra 1 #6928), `usage.invalid_arguments` on `msg wait` (#8185,
    /// Terra 2 #8457).
    /// `Last-Event-ID` is never read, since the signing profile doesn't
    /// cover it and a party in the path could move the cursor (Decided,
    /// #6673; clients resume with `?after=<cursor>`).
    fn stream_after(&self) -> Result<Option<String>, Refusal> {
        let cursor = || Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, "cursor");
        let mut after = None;
        for (name, value) in self.query_pairs()? {
            if name != "after" {
                continue;
            }
            if after.is_some() {
                return Err(cursor());
            }
            after = Some(value);
        }
        Ok(after)
    }

    /// The declared body length: `None` without a body. A repeated or
    /// malformed `Content-Length`, and any `Transfer-Encoding` (clients
    /// send bodies with a length, so no chunked framing reaches the
    /// limits unmeasured), are refused; a number past `u64` is too large.
    fn body_length(&self) -> Result<Option<u64>, Length> {
        if self.has("transfer-encoding") {
            return Err(Length::Invalid);
        }
        let Some(v) = self.one("content-length").map_err(|()| Length::Invalid)? else {
            return Ok(None);
        };
        if v.is_empty() || !v.bytes().all(|c| c.is_ascii_digit()) {
            return Err(Length::Invalid);
        }
        v.parse::<u64>().map(Some).map_err(|_| Length::TooLarge)
    }
}

/// Why a body length was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Length {
    /// Repeated, not a decimal number, or chunked: 400.
    Invalid,
    /// Past `u64`: 413.
    TooLarge,
}

/// Percent-decodes one query name or value; `+` is a space.
fn decode(raw: &str) -> Result<String, Refusal> {
    let bad = || Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, "query");
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = raw.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hi = b.get(i + 1).copied().and_then(hex).ok_or_else(bad)?;
                let lo = b.get(i + 2).copied().and_then(hex).ok_or_else(bad)?;
                out.push(hi << 4 | lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_resumes_only_from_the_signed_after() {
        let mut head = RequestHead {
            method: "GET".into(),
            path: "/v1/events".into(),
            query: Some("kind=lease&after=s1:42".into()),
            headers: vec![("last-event-id".into(), "s1:99".into())],
        };
        assert_eq!(head.stream_after(), Ok(Some("s1:42".into())));
        head.query = Some("afterx=1&kind=lease".into());
        assert_eq!(head.stream_after(), Ok(None), "Last-Event-ID is never read");
        head.query = None;
        assert_eq!(head.stream_after(), Ok(None));
    }

    #[test]
    fn a_resume_cursor_is_decoded_and_never_read_as_absent_when_invalid() {
        let after = |q: &str| {
            RequestHead {
                method: "GET".into(),
                path: "/v1/events".into(),
                query: Some(q.into()),
                headers: vec![],
            }
            .stream_after()
            .map_err(|r| r.test_reason())
        };
        // A query encoder's `%3A`, and an encoded name, are the cursor.
        assert_eq!(after("after=t-1%3A42"), Ok(Some("t-1:42".into())));
        assert_eq!(after("%61fter=s1:7&kind=x"), Ok(Some("s1:7".into())));
        assert_eq!(after("kind=a+b&after=s1%3a7"), Ok(Some("s1:7".into())));
        // Invalid is refused, not absent: absence would start at the head.
        assert_eq!(after("after=s1%3"), Err("query"));
        assert_eq!(after("after=s1%zz"), Err("query"));
        assert_eq!(after("after=%ff"), Err("query"));
        assert_eq!(after("kind=%"), Err("query"));
        // Empty is a cursor for the reader to refuse (api.bad_cursor with
        // the head), never absent (Terra 1 #6928).
        assert_eq!(after("after="), Ok(Some(String::new())));
        assert_eq!(after("after"), Ok(Some(String::new())));
        assert_eq!(after("after=&after=s1:2"), Err("cursor"));
        assert_eq!(after("after=s1:1&after=s1:2"), Err("cursor"));
        assert_eq!(after("after=s1:1&%61fter=s1:2"), Err("cursor"));
        assert_eq!(after("kind=x&&"), Ok(None));
    }

    #[test]
    fn body_lengths() {
        let with = |h: &[(&str, &str)]| RequestHead {
            method: "POST".into(),
            path: "/".into(),
            query: None,
            headers: h
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        };
        assert_eq!(with(&[]).body_length(), Ok(None));
        assert_eq!(
            with(&[("content-length", "12")]).body_length(),
            Ok(Some(12))
        );
        for bad in ["", "-1", "1 2", "0x10", "+5"] {
            assert_eq!(
                with(&[("content-length", bad)]).body_length(),
                Err(Length::Invalid),
                "{bad}"
            );
        }
        assert_eq!(
            with(&[("content-length", "1"), ("content-length", "1")]).body_length(),
            Err(Length::Invalid)
        );
        assert_eq!(
            with(&[("transfer-encoding", "chunked")]).body_length(),
            Err(Length::Invalid)
        );
        assert_eq!(
            with(&[("content-length", "99999999999999999999999")]).body_length(),
            Err(Length::TooLarge)
        );
    }

    #[test]
    fn singletons() {
        let head = RequestHead {
            method: "GET".into(),
            path: "/".into(),
            query: None,
            headers: vec![
                ("a".into(), "1".into()),
                ("b".into(), "1".into()),
                ("b".into(), "2".into()),
            ],
        };
        assert_eq!(head.one("a"), Ok(Some("1")));
        assert_eq!(head.one("b"), Err(()));
        assert_eq!(head.one("c"), Ok(None));
    }
}
