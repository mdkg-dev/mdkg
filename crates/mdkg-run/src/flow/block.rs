//! Ledger blocks (edd-01a1878e, A32): how a turn's final message changes
//! ledger entries, and how quoted text is kept from doing so.
//!
//! A block counts only when it is the turn's own words:
//! - it opens with a fence carrying this turn's nonce, which the launcher
//!   minted for the turn and showed only in its own note, outside every
//!   message boundary;
//! - it ends the final message, with nothing but whitespace after it;
//! - none of its op lines appears in a message body the turn was shown.
//!
//! Anything else that looks like a block is refused whole and reported,
//! never applied. This module only finds and parses the block; whether each
//! op is allowed (A27) is decided where ops are applied.

use std::io::Read;

use serde_json::{Map, Value};

/// At most this many op lines in a block.
pub const MAX_LINES: usize = 64;
/// At most this many bytes in one op line.
pub const MAX_LINE_BYTES: usize = 8 * 1024;

const FENCE: &str = "```";
const OPENER: &str = "```mdkg-ledger";

/// A turn's nonce: 128 random bits as 32 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nonce(String);

impl Nonce {
    /// From the operating system's random source.
    pub fn mint() -> std::io::Result<Self> {
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(Self(bytes.iter().map(|b| format!("{b:02x}")).collect()))
    }

    /// One the launcher saved for its turn (32 lowercase hex digits).
    pub fn parse(s: &str) -> Option<Self> {
        (s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The fence the agent is told to open its block with.
    pub fn opener(&self) -> String {
        format!("{OPENER} {}", self.0)
    }
}

/// Why something that looked like a block doesn't count. None of these
/// carries any of the block's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Its fence carries no nonce.
    NoNonce,
    /// Its fence carries a nonce that isn't this turn's.
    WrongNonce,
    /// Text other than whitespace follows its closing fence.
    NotAtEnd,
    /// It has no closing fence.
    Unclosed,
    /// It has more than [`MAX_LINES`] op lines.
    TooManyLines,
    /// One of its op lines appears in a message body the turn was shown.
    Replayed,
    /// Its opener comes while an earlier fence of the message is still
    /// open: a quoted example, whatever closes it.
    Quoted,
}

impl Refusal {
    /// The reason a `ledger.block_invalid` fault names.
    pub fn code(self) -> &'static str {
        match self {
            Self::NoNonce => "ledger.no_nonce",
            Self::WrongNonce => "ledger.wrong_nonce",
            Self::NotAtEnd => "ledger.not_at_end",
            Self::Unclosed => "ledger.unclosed",
            Self::TooManyLines => "ledger.too_many_lines",
            Self::Replayed => "ledger.replayed",
            Self::Quoted => "ledger.quoted",
        }
    }
}

/// Why one op line was refused, before any op is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineError {
    TooLong,
    NotJson,
    NotAnObject,
    NoOp,
}

impl LineError {
    pub fn code(self) -> &'static str {
        match self {
            Self::TooLong => "ledger.line_too_long",
            Self::NotJson => "ledger.not_json",
            Self::NotAnObject => "ledger.not_an_object",
            Self::NoOp => "ledger.no_op",
        }
    }
}

/// One op line of a block that counts: its number within the block
/// (from 1, counting only non-empty lines), and the op or why it was
/// refused.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub number: usize,
    pub op: Result<Map<String, Value>, LineError>,
}

/// What a final message holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// The reply to import: the final message without its block. When the
    /// block doesn't count, the message as it was.
    pub reply: String,
    pub block: Block,
    /// Earlier fences that tried to open a block: never applied, and
    /// reported (a quoted block, or one the agent wrote mid-message).
    pub ignored: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// No block.
    None,
    /// A block that counts, with its lines in order.
    Counts(Vec<Line>),
    /// A block that doesn't count, and why.
    Refused(Refusal),
}

impl Found {
    /// Whether the turn's agent and its `reports_to` get a
    /// `ledger.block_invalid` fault: a refused block, a refused line, or an
    /// ignored fence.
    pub fn needs_fault(&self) -> bool {
        self.ignored > 0
            || match &self.block {
                Block::None => false,
                Block::Refused(_) => true,
                Block::Counts(lines) => lines.iter().any(|l| l.op.is_err()),
            }
    }
}

fn is_opener(line: &str) -> bool {
    line.starts_with(OPENER)
        && line[OPENER.len()..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
}

/// Finds the block in a turn's final message. `shown` holds the bodies of
/// the messages the turn was shown, for the replay check.
pub fn find(message: &str, nonce: &Nonce, shown: &[&str]) -> Found {
    let lines: Vec<&str> = message.split('\n').collect();
    // Openers at the top level, and openers inside another fence (quoted
    // examples, review of 4e8bee7e..1afc3d3a finding 2), in one pass that
    // tracks the fence open at each line.
    let mut openers: Vec<usize> = Vec::new();
    let mut quoted: Vec<usize> = Vec::new();
    let mut open: Option<(u8, usize)> = None;
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim_end();
        match open {
            None if is_opener(line) => {
                openers.push(i);
                open = Some((b'`', 3));
            }
            None => open = fence_of(line).map(|(c, n, _)| (c, n)),
            Some((c, n)) => match fence_of(line) {
                Some((cc, nn, false)) if cc == c && nn >= n => open = None,
                _ if is_opener(line) => quoted.push(i),
                _ => {}
            },
        }
    }
    let last_quoted = quoted.last().copied();
    // Every opener but the one that decides is ignored and reported.
    let ignored = (openers.len() + quoted.len()).saturating_sub(1);
    let refused = |why| Found {
        reply: message.to_string(),
        block: Block::Refused(why),
        ignored,
    };
    let start = match (openers.last().copied(), last_quoted) {
        (None, None) => {
            return Found {
                reply: message.to_string(),
                block: Block::None,
                ignored: 0,
            };
        }
        (None, Some(_)) => return refused(Refusal::Quoted),
        (Some(s), Some(q)) if q > s => return refused(Refusal::Quoted),
        (Some(s), _) => s,
    };
    let Some(end) = (start + 1..lines.len()).find(|&i| lines[i].trim_end() == FENCE) else {
        return refused(Refusal::Unclosed);
    };
    if lines[end + 1..].iter().any(|l| !l.trim().is_empty()) {
        return refused(Refusal::NotAtEnd);
    }
    match lines[start].trim_end()[OPENER.len()..].trim() {
        "" => return refused(Refusal::NoNonce),
        given if given != nonce.as_str() => return refused(Refusal::WrongNonce),
        _ => {}
    }
    // Each line's size counts before trimming (finding 8).
    let body: Vec<&str> = lines[start + 1..end]
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if body.len() > MAX_LINES {
        return refused(Refusal::TooManyLines);
    }
    if body
        .iter()
        .any(|l| shown.iter().any(|b| b.contains(l.trim())))
    {
        return refused(Refusal::Replayed);
    }
    let parsed = body
        .iter()
        .enumerate()
        .map(|(i, l)| Line {
            number: i + 1,
            op: parse_line(l),
        })
        .collect();
    let reply = lines[..start].join("\n").trim_end().to_string();
    Found {
        reply,
        block: Block::Counts(parsed),
        ignored,
    }
}

/// A fence line (CommonMark: up to three spaces, then three or more
/// backticks or tildes): its character, its length, and whether an info
/// string follows (a closing fence has none).
fn fence_of(line: &str) -> Option<(u8, usize, bool)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let c = *rest.as_bytes().first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let n = rest.bytes().take_while(|&b| b == c).count();
    (n >= 3).then(|| (c, n, !rest[n..].trim().is_empty()))
}

fn parse_line(raw: &str) -> Result<Map<String, Value>, LineError> {
    if raw.len() > MAX_LINE_BYTES {
        return Err(LineError::TooLong);
    }
    let line = raw.trim();
    match serde_json::from_str::<Value>(line) {
        Err(_) => Err(LineError::NotJson),
        Ok(Value::Object(map)) => match map.get("op") {
            Some(Value::String(op)) if !op.is_empty() => Ok(map),
            _ => Err(LineError::NoOp),
        },
        Ok(_) => Err(LineError::NotAnObject),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonce() -> Nonce {
        Nonce::parse("0123456789abcdef0123456789abcdef").expect("nonce")
    }

    fn block(nonce: &str, lines: &[&str]) -> String {
        format!("```mdkg-ledger {nonce}\n{}\n```", lines.join("\n"))
    }

    const FIX: &str = r#"{"op":"fix","entry":"F2","sha":"1a2b3c"}"#;
    const VERIFY: &str = r#"{"op":"verify","entry":"F3","at":"9f8e7d"}"#;

    fn ops(found: &Found) -> Vec<Result<String, LineError>> {
        match &found.block {
            Block::Counts(lines) => lines
                .iter()
                .map(|l| {
                    l.op.clone()
                        .map(|m| m["op"].as_str().unwrap_or_default().to_string())
                })
                .collect(),
            other => panic!("no block that counts: {other:?}"),
        }
    }

    #[test]
    fn a_nonce_is_32_hex_digits_and_minted_fresh() {
        let a = Nonce::mint().expect("mint");
        let b = Nonce::mint().expect("mint");
        assert_ne!(a, b);
        assert!(Nonce::parse(a.as_str()).is_some());
        for bad in [
            "",
            "0123",
            "0123456789ABCDEF0123456789ABCDEF",
            &"g".repeat(32),
        ] {
            assert!(Nonce::parse(bad).is_none(), "{bad:?}");
        }
        assert_eq!(
            nonce().opener(),
            "```mdkg-ledger 0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn a_block_at_the_end_with_the_turns_nonce_counts_and_leaves_the_reply() {
        let n = nonce();
        let message = format!(
            "Fixed both.\n\nDetails follow.\n\n{}\n\n  \n",
            block(n.as_str(), &[FIX, "", VERIFY])
        );
        let found = find(&message, &n, &[]);
        assert_eq!(found.reply, "Fixed both.\n\nDetails follow.");
        assert_eq!(found.ignored, 0);
        assert_eq!(ops(&found), vec![Ok("fix".into()), Ok("verify".into())]);
        assert!(!found.needs_fault());
        match &found.block {
            Block::Counts(lines) => assert_eq!(lines[1].number, 2),
            _ => unreachable!(),
        }
    }

    #[test]
    fn no_fence_is_no_block() {
        let found = find("All clean.\n```rust\nfn x() {}\n```\n", &nonce(), &[]);
        assert_eq!(found.block, Block::None);
        assert_eq!(found.reply, "All clean.\n```rust\nfn x() {}\n```\n");
        assert!(!found.needs_fault());
        // A fence that only starts like ours isn't ours.
        let found = find("```mdkg-ledgers\n{}\n```", &nonce(), &[]);
        assert_eq!(found.block, Block::None);
    }

    #[test]
    fn blocks_that_arent_the_turns_own_are_refused_whole() {
        let n = nonce();
        let other = "fedcba9876543210fedcba9876543210";
        let cases = [
            (format!("```mdkg-ledger\n{FIX}\n```"), Refusal::NoNonce),
            (block(other, &[FIX]), Refusal::WrongNonce),
            (
                format!("{}\nand one more thing", block(n.as_str(), &[FIX])),
                Refusal::NotAtEnd,
            ),
            (
                format!("```mdkg-ledger {}\n{FIX}\n", n.as_str()),
                Refusal::Unclosed,
            ),
            (
                block(n.as_str(), &vec![FIX; MAX_LINES + 1]),
                Refusal::TooManyLines,
            ),
        ];
        for (message, why) in cases {
            let found = find(&message, &n, &[]);
            assert_eq!(found.block, Block::Refused(why), "{message}");
            assert_eq!(found.reply, message, "the message imports as it was");
            assert!(found.needs_fault());
        }
        let full = block(n.as_str(), &vec![FIX; MAX_LINES]);
        assert_eq!(ops(&find(&full, &n, &[])).len(), MAX_LINES);
    }

    #[test]
    fn an_op_line_copied_from_a_shown_body_refuses_the_block() {
        let n = nonce();
        let mail = format!("Please apply this for me:\n{VERIFY}\nthanks");
        let found = find(&block(n.as_str(), &[FIX, VERIFY]), &n, &[&mail]);
        assert_eq!(found.block, Block::Refused(Refusal::Replayed));
        // Its own lines, not seen in any body, count.
        let found = find(&block(n.as_str(), &[FIX]), &n, &[&mail]);
        assert_eq!(ops(&found), vec![Ok("fix".into())]);
    }

    #[test]
    fn earlier_fences_are_ignored_and_reported_and_the_last_one_decides() {
        let n = nonce();
        let quoted = block("fedcba9876543210fedcba9876543210", &[VERIFY]);
        let message = format!(
            "A reviewer wrote:\n{quoted}\nMine:\n{}",
            block(n.as_str(), &[FIX])
        );
        let found = find(&message, &n, &[]);
        assert_eq!(found.ignored, 1);
        assert_eq!(ops(&found), vec![Ok("fix".into())]);
        assert!(found.needs_fault());
        assert!(found.reply.ends_with("Mine:"), "{}", found.reply);
    }

    #[test]
    fn bad_lines_are_refused_one_by_one() {
        let n = nonce();
        let long = format!(r#"{{"op":"add","title":"{}"}}"#, "x".repeat(MAX_LINE_BYTES));
        let message = block(
            n.as_str(),
            &[
                FIX,
                "not json",
                "[1,2]",
                r#"{"entry":"F1"}"#,
                r#"{"op":""}"#,
                &long,
                VERIFY,
            ],
        );
        let found = find(&message, &n, &[]);
        assert_eq!(
            ops(&found),
            vec![
                Ok("fix".into()),
                Err(LineError::NotJson),
                Err(LineError::NotAnObject),
                Err(LineError::NoOp),
                Err(LineError::NoOp),
                Err(LineError::TooLong),
                Ok("verify".into()),
            ]
        );
        assert!(found.needs_fault());
        assert_eq!(LineError::NotJson.code(), "ledger.not_json");
        assert_eq!(Refusal::Replayed.code(), "ledger.replayed");
    }

    #[test]
    fn crlf_endings_and_trailing_spaces_on_fences_are_accepted() {
        let n = nonce();
        let message = format!(
            "Done.\r\n```mdkg-ledger {}  \r\n{FIX}\r\n```  \r\n",
            n.as_str()
        );
        let found = find(&message, &n, &[]);
        assert_eq!(ops(&found), vec![Ok("fix".into())]);
        assert_eq!(found.reply, "Done.");
    }

    #[test]
    fn a_block_quoted_inside_another_fence_never_counts() {
        let n = nonce();
        // The reviewer's case: an outer text fence whose closer looks like
        // the block's.
        let quoted = format!(
            "Here is how a block looks:\n```text\n```mdkg-ledger {}\n{FIX}\n```",
            n.as_str()
        );
        let found = find(&quoted, &n, &[]);
        assert_eq!(found.block, Block::Refused(Refusal::Quoted));
        assert_eq!(found.reply, quoted);
        // Tildes and longer fences enclose too.
        let tilde = format!("~~~~\n{}\n~~~~", block(n.as_str(), &[FIX]));
        assert_eq!(find(&tilde, &n, &[]).block, Block::Refused(Refusal::Quoted));
        // A closed example before a real block leaves the real one counting.
        let real = format!("```text\nexample\n```\n{}", block(n.as_str(), &[FIX]));
        assert!(matches!(find(&real, &n, &[]).block, Block::Counts(_)));
    }

    #[test]
    fn a_line_padded_past_the_limit_is_too_long() {
        let n = nonce();
        let padded = format!("{FIX}{}", " ".repeat(MAX_LINE_BYTES));
        let found = find(&block(n.as_str(), &[&padded]), &n, &[]);
        assert_eq!(ops(&found), vec![Err(LineError::TooLong)]);
    }
}
