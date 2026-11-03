//! The subset of RFC 8941 structured fields the signing profile uses:
//! dictionaries whose members are inner lists of strings with parameters,
//! or byte sequences. Parsing is strict; serialization is canonical, and
//! builds RFC 9421's `@signature-params` line and the person signature's
//! covered members.

use crate::b64;

/// A bare item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Integer(i64),
    String(String),
    Token(String),
    Bytes(Vec<u8>),
}

pub type Params = Vec<(String, Item)>;

/// An inner list's item: the item and its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub item: Item,
    pub params: Params,
}

/// A dictionary member's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Member {
    Item(Entry),
    InnerList(Vec<Entry>, Params),
}

pub type Dictionary = Vec<(String, Member)>;

/// Bounds that hold the parse to linear work on any header (Terra 6
/// #6667): the profile needs at most five parameters, nine components and
/// two members, so anything past these is refused as it is read.
pub const MAX_PARAMS: usize = 8;
pub const MAX_LIST: usize = 16;
pub const MAX_MEMBERS: usize = 4;

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn sp(&mut self) {
        while self.peek() == Some(b' ') {
            self.i += 1;
        }
    }

    fn ows(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.i += 1;
        }
    }

    fn key(&mut self) -> Option<String> {
        let start = self.i;
        match self.peek() {
            Some(c) if c.is_ascii_lowercase() || c == b'*' => self.i += 1,
            _ => return None,
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || matches!(c, b'_' | b'-' | b'.' | b'*')
            {
                self.i += 1;
            } else {
                break;
            }
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()
            .map(str::to_owned)
    }

    fn item(&mut self) -> Option<Item> {
        match self.peek()? {
            b'"' => {
                self.i += 1;
                let mut out = String::new();
                loop {
                    let c = self.peek()?;
                    self.i += 1;
                    match c {
                        b'\\' => {
                            let n = self.peek()?;
                            if n != b'"' && n != b'\\' {
                                return None;
                            }
                            self.i += 1;
                            out.push(n as char);
                        }
                        b'"' => return Some(Item::String(out)),
                        0x20..=0x7e => out.push(c as char),
                        _ => return None,
                    }
                }
            }
            b':' => {
                self.i += 1;
                let start = self.i;
                while self.peek()? != b':' {
                    self.i += 1;
                }
                let text = std::str::from_utf8(&self.s[start..self.i]).ok()?;
                self.i += 1;
                b64::decode(text).map(Item::Bytes)
            }
            b'-' | b'0'..=b'9' => {
                let start = self.i;
                self.eat(b'-');
                let digits = self.i;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.i += 1;
                }
                // Integers only, at most 15 digits (RFC 8941 3.3.1).
                if self.i == digits || self.i - digits > 15 || self.peek() == Some(b'.') {
                    return None;
                }
                std::str::from_utf8(&self.s[start..self.i])
                    .ok()?
                    .parse()
                    .ok()
                    .map(Item::Integer)
            }
            c if c.is_ascii_alphabetic() || c == b'*' => {
                let start = self.i;
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~:/".contains(&c) {
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                std::str::from_utf8(&self.s[start..self.i])
                    .ok()
                    .map(|t| Item::Token(t.to_owned()))
            }
            _ => None,
        }
    }

    fn params(&mut self) -> Option<Params> {
        let mut out: Params = Vec::new();
        while self.eat(b';') {
            if out.len() == MAX_PARAMS {
                return None;
            }
            self.sp();
            let k = self.key()?;
            let v = if self.eat(b'=') {
                self.item()?
            } else {
                Item::Token("?1".into())
            };
            // A repeated parameter is refused rather than overwritten.
            if out.iter().any(|(n, _)| *n == k) {
                return None;
            }
            out.push((k, v));
        }
        Some(out)
    }

    fn entry(&mut self) -> Option<Entry> {
        let item = self.item()?;
        let params = self.params()?;
        Some(Entry { item, params })
    }

    fn member(&mut self) -> Option<Member> {
        if self.eat(b'(') {
            let mut list = Vec::new();
            loop {
                self.sp();
                if self.eat(b')') {
                    break;
                }
                if list.len() == MAX_LIST {
                    return None;
                }
                list.push(self.entry()?);
                if !matches!(self.peek(), Some(b' ' | b')')) {
                    return None;
                }
            }
            Some(Member::InnerList(list, self.params()?))
        } else {
            Some(Member::Item(self.entry()?))
        }
    }
}

/// Parses a dictionary. A repeated key is refused, unlike RFC 8941's
/// last-wins, so no member is read from one copy and verified from another.
pub fn parse_dictionary(text: &str) -> Option<Dictionary> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    let mut out: Dictionary = Vec::new();
    p.sp();
    loop {
        if out.len() == MAX_MEMBERS {
            return None;
        }
        let k = p.key()?;
        if !p.eat(b'=') {
            return None;
        }
        let m = p.member()?;
        if out.iter().any(|(n, _)| *n == k) {
            return None;
        }
        out.push((k, m));
        p.ows();
        if p.i == p.s.len() {
            return Some(out);
        }
        if !p.eat(b',') {
            return None;
        }
        p.ows();
        if p.i == p.s.len() {
            return None;
        }
    }
}

fn write_item(out: &mut String, item: &Item) {
    match item {
        Item::Integer(n) => out.push_str(&n.to_string()),
        Item::String(s) => {
            out.push('"');
            for c in s.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        }
        Item::Token(t) => out.push_str(t),
        Item::Bytes(b) => {
            out.push(':');
            out.push_str(&b64::encode(b));
            out.push(':');
        }
    }
}

fn write_params(out: &mut String, params: &Params) {
    for (k, v) in params {
        out.push(';');
        out.push_str(k);
        if v != &Item::Token("?1".into()) {
            out.push('=');
            write_item(out, v);
        }
    }
}

/// A member's canonical serialization.
pub fn serialize_member(m: &Member) -> String {
    let mut out = String::new();
    match m {
        Member::Item(e) => {
            write_item(&mut out, &e.item);
            write_params(&mut out, &e.params);
        }
        Member::InnerList(list, params) => {
            out.push('(');
            for (i, e) in list.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                write_item(&mut out, &e.item);
                write_params(&mut out, &e.params);
            }
            out.push(')');
            write_params(&mut out, params);
        }
    }
    out
}

/// A parameter's value.
pub fn param<'a>(params: &'a Params, name: &str) -> Option<&'a Item> {
    params.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_reserializes_a_signature_input() {
        let text = r#"mdkg=("@method" "@path" "mdkg-graph");created=1793290515;keyid="018f";nonce="q2X";alg="ed25519";tag="mdkg-v1""#;
        let d = parse_dictionary(text).expect("parses");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0, "mdkg");
        assert_eq!(format!("mdkg={}", serialize_member(&d[0].1)), text);
        let Member::InnerList(list, params) = &d[0].1 else {
            panic!("inner list")
        };
        assert_eq!(list.len(), 3);
        assert_eq!(
            param(params, "created"),
            Some(&Item::Integer(1_793_290_515))
        );
    }

    #[test]
    fn byte_sequences_and_component_params() {
        let d = parse_dictionary(r#"mdkg=:AAEC:, mdkg-person=("signature";key="mdkg")"#)
            .expect("parses");
        assert_eq!(
            d[0].1,
            Member::Item(Entry {
                item: Item::Bytes(vec![0, 1, 2]),
                params: vec![]
            })
        );
        assert_eq!(serialize_member(&d[1].1), r#"("signature";key="mdkg")"#);
    }

    #[test]
    fn refuses_repeats_and_junk() {
        for bad in [
            r#"mdkg=("a"), mdkg=("b")"#,
            r#"mdkg=("a");created=1;created=2"#,
            r#"mdkg=("a"),"#,
            r#"mdkg=("a"#,
            r#"Mdkg=("a")"#,
            r#"mdkg=("a\x")"#,
            r#"mdkg=("a");created=1.5"#,
            r#"mdkg=("a");created=1234567890123456"#,
            r#"mdkg=:not base64!:"#,
            r#"mdkg=("a")("b")"#,
        ] {
            assert_eq!(parse_dictionary(bad), None, "{bad}");
        }
    }

    #[test]
    fn long_headers_are_cut_off_early() {
        let params: String = (0..2100).map(|i| format!(";p{i}=1")).collect();
        assert_eq!(parse_dictionary(&format!("mdkg=(\"a\"){params}")), None);
        let eight: String = (0..8).map(|i| format!(";p{i}=1")).collect();
        assert!(parse_dictionary(&format!("mdkg=(\"a\"){eight}")).is_some());
        let list: String = (0..17).map(|i| format!("\"c{i}\" ")).collect();
        assert_eq!(
            parse_dictionary(&format!("mdkg=({})", list.trim_end())),
            None
        );
        let members: String = (0..5)
            .map(|i| format!("k{i}=1"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(parse_dictionary(&members), None);
    }

    #[test]
    fn escapes_round_trip() {
        let d = parse_dictionary(r#"k=("a\"b\\c")"#).expect("parses");
        assert_eq!(serialize_member(&d[0].1), r#"("a\"b\\c")"#);
    }
}
