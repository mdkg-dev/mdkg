//! Shared high-confidence credential detection for messages and configuration.

/// High-confidence credential shapes: a private key block, a known token
/// prefix followed by a long token, a JWT, a URL with a password, or a
/// `password`/`token` assignment with a long value ([`has_assignment`]).
///
/// An escaped control or space character (as JSON, logs, string literals
/// and URLs write them) separates words as the character it stands for
/// does, so `literal\nghp_…` is a token after a newline, not after the
/// word `nghp`. See [`unescape_whitespace`] for the spellings read.
/// URL encoding can wrap any of them (`%5Cn` is `\n`, `%250A` is `%0A`),
/// so each decoded layer, up to [`PERCENT_LAYERS`] of them, is read the
/// same way (e2e B10). A URL is read for its password once, on the
/// layer where its `://` first appears: as written, or on the layer
/// whose decoding made it (a URL encoded whole, review of bf42c75e). A
/// later layer decodes that URL's own `%3A` into a `:` that delimits
/// nothing, so it isn't read again (reviews of c0130d38 and 154d5016);
/// another URL elsewhere in the text doesn't change which are read.
pub(crate) fn looks_secret(text: &str) -> bool {
    let mut layer = std::borrow::Cow::Borrowed(text);
    // `None` on the text as written: every URL is new there.
    let mut revealed: Option<Vec<usize>> = None;
    for depth in 0..=PERCENT_LAYERS {
        let urls = match &revealed {
            None => Urls::All,
            Some(at) => Urls::At(at),
        };
        // Unescaping only makes spaces, which reveal no URL: its URLs
        // were read on the layer itself.
        let unescaped = if depth == 0 { Urls::All } else { Urls::None };
        if shaped(&layer, urls)
            || unescape_whitespace(&layer).is_some_and(|text| shaped(&text, unescaped))
        {
            return true;
        }
        match percent_decode(&layer) {
            Some((next, at)) => {
                layer = std::borrow::Cow::Owned(next);
                revealed = Some(at);
            }
            None => return false,
        }
    }
    false
}

/// Which of a text's URLs [`shaped`] reads for a password.
#[derive(Clone, Copy)]
enum Urls<'a> {
    All,
    None,
    /// Only those whose `://` starts at one of these byte offsets.
    At(&'a [usize]),
}

/// How many layers of URL encoding [`looks_secret`] peels off.
const PERCENT_LAYERS: usize = 4;

/// `text` with every `%XX` decoded, or `None` when it has none, and the
/// byte offsets of each `://` in the result that a decoded byte is part
/// of: the URLs this layer revealed. Bytes that don't make UTF-8 become
/// U+FFFD, which joins no word; offsets then can't be mapped, so every
/// `://` counts as revealed.
fn percent_decode(text: &str) -> Option<(String, Vec<usize>)> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        let byte = (bytes[i] == b'%')
            .then(|| text.get(i + 1..i + 3))
            .flatten()
            .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match byte {
            Some(byte) => {
                out.push(byte);
                decoded.push(true);
                changed = true;
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                decoded.push(false);
                i += 1;
            }
        }
    }
    if !changed {
        return None;
    }
    let schemes = |text: &[u8]| -> Vec<usize> {
        text.windows(3)
            .enumerate()
            .filter(|(_, w)| *w == b"://")
            .map(|(at, _)| at)
            .collect()
    };
    Some(match String::from_utf8(out) {
        Ok(next) => {
            let at = schemes(next.as_bytes())
                .into_iter()
                .filter(|&at| decoded[at..at + 3].contains(&true))
                .collect();
            (next, at)
        }
        Err(e) => {
            let next = String::from_utf8_lossy(e.as_bytes()).into_owned();
            let at = schemes(next.as_bytes());
            (next, at)
        }
    })
}

/// `text` with each escaped control or space character turned into a
/// space, or `None` when it has none: a C or JSON escape (`\n`, `\r`,
/// `\t`, `\f`, `\v`, `\b`, `\0` with up to two more octal digits), a
/// `\xXX` or `\uXXXX` escape, or URL encoding (`%XX`); a numeric one only
/// for a byte up to 0x20, or for `\x`/`\u` any whitespace character
/// (`\u2028`, `\u00a0`, `\x85`). Other escapes are left as they are.
fn unescape_whitespace(text: &str) -> Option<String> {
    if !text.contains(['\\', '%']) {
        return None;
    }
    let bytes = text.as_bytes();
    // A hex value at `at`, `digits` long, when it's a control or a space,
    // or with `wide` any whitespace character.
    let control = |at: usize, digits: usize, wide: bool| {
        let hex = text.get(at..at + digits)?;
        let value = u32::from_str_radix(hex, 16).ok()?;
        let space =
            value <= 0x20 || (wide && char::from_u32(value).is_some_and(char::is_whitespace));
        (hex.bytes().all(|b| b.is_ascii_hexdigit()) && space).then_some(at + digits)
    };
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        let next = bytes.get(i + 1).copied();
        let end = match (bytes[i], next) {
            (b'\\', Some(b'n' | b'r' | b't' | b'f' | b'v' | b'b')) => Some(i + 2),
            // `\0` and up to two more octal digits, when the value is a
            // control or a space (`\060` is a printable `0`).
            (b'\\', Some(b'0')) => {
                let end = (i + 2..(i + 4).min(bytes.len()))
                    .take_while(|&j| (b'0'..=b'7').contains(&bytes[j]))
                    .last()
                    .map_or(i + 2, |j| j + 1);
                let value = bytes[i + 1..end]
                    .iter()
                    .fold(0u32, |v, b| v * 8 + u32::from(b - b'0'));
                (value <= 0x20).then_some(end)
            }
            (b'\\', Some(b'x')) => control(i + 2, 2, true),
            (b'\\', Some(b'u')) => control(i + 2, 4, true),
            (b'%', _) => control(i + 1, 2, false),
            _ => None,
        };
        match end {
            Some(end) => {
                out.push(' ');
                changed = true;
                i = end;
            }
            None => {
                // `text` is UTF-8 and every marker is ASCII, so this char
                // starts at `i`.
                let c = text[i..].chars().next().expect("a char at a boundary");
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    changed.then_some(out)
}

/// The shapes themselves, on text as it stands. A URL with a password
/// counts too, for the URLs `urls` names.
fn shaped(text: &str, urls: Urls<'_>) -> bool {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return true;
    }
    if has_jwt(text)
        || has_url_password(text, urls)
        || has_exact_token(text)
        || has_assignment(text)
    {
        return true;
    }
    const PREFIXES: &[(&str, usize)] = &[
        ("sk-", 20),
        ("sk-ant-", 20),
        ("sk-proj-", 20),
        ("ghp_", 20),
        ("ghu_", 20),
        ("ghr_", 20),
        ("gho_", 20),
        ("ghs_", 20),
        ("github_pat_", 20),
        ("xoxa-", 20),
        ("xoxr-", 20),
        ("xoxb-", 20),
        ("xoxp-", 20),
        ("AIza", 30),
        ("glpat-", 20),
        ("pypi-", 30),
        ("sk_org_", 24),
        ("whsec_", 24),
    ];
    let token = |s: &str| {
        s.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .count()
    };
    PREFIXES.iter().any(|(prefix, min)| {
        text.match_indices(prefix).any(|(i, _)| {
            // Any letter or digit joins a word, not only an ASCII one, so
            // `tásk-…` isn't `sk-…` after a boundary, decomposed or not;
            // nor does a `-` or `_` between it and a letter or digit
            // (`my-sk-…`, `x_sk_org_…`, an identifier's separators). One
            // that starts a word still does (`_<key>_` for emphasis, ` -<key>`).
            // The boundary first, so a long run of prefixes inside one
            // word isn't scanned to its end at each (Terra #18248).
            if joins_before(text[..i].trim_end_matches(['-', '_'])) {
                return false;
            }
            let rest = &text[i + prefix.len()..];
            let n = token(rest);
            n >= *min && key_body(prefix, &rest[..n])
        })
    })
}

/// Whether what follows a prefix can be a key. After a bare `sk-`, a UUID
/// alone (8-4-4-4-12 hex digits) is the tail of an id quoted on its own
/// (`sk-01a1a432-73ed-…` from `task-01a1a432-73ed-…`), which refused a
/// review's reply (orchestrator, 2026-11-03); any other body, unhyphenated
/// hex included, still counts (Terra #18248).
fn key_body(prefix: &str, body: &str) -> bool {
    prefix != "sk-" || !is_uuid(body)
}

/// `8-4-4-4-12` hex digits, either case, and nothing else. The length
/// first, then the bytes in place: a huge token allocates nothing (Terra
/// #18301).
fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// Whether the text before a token ends inside a word: its last base
/// character, past any combining marks (NFD text writes `á` as `a` and
/// U+0301), is a letter or digit (review of c6df840d).
fn joins_before(before: &str) -> bool {
    before
        .chars()
        .rev()
        .find(|&c| !combining(c))
        .is_some_and(char::is_alphanumeric)
}

/// Whether `c` is a combining mark, in the blocks that hold them:
/// Combining Diacritical Marks and their Extended and Supplement blocks,
/// those for Symbols, and Combining Half Marks.
fn combining(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// Whether `c` can be in a token: base64url's alphabet.
fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

/// Tokens with an exact shape, so an ordinary identifier that merely
/// starts like one (`ASIA_PACIFIC_…`, `npm_config_…`,
/// `is_sk_live_mode`) isn't one: an AWS key id (`AKIA`/`ASIA` and exactly
/// 16 of A-Z, 0-9), an npm token (`npm_` and at least 30 of A-Z, a-z,
/// 0-9) or a Stripe secret/restricted key (24 or more of them), with
/// no letter or digit right before or after.
fn has_exact_token(text: &str) -> bool {
    type Shape = (&'static str, usize, bool, fn(char) -> bool);
    const SHAPES: &[Shape] = &[
        ("AKIA", 16, true, |c| {
            c.is_ascii_uppercase() || c.is_ascii_digit()
        }),
        ("ASIA", 16, true, |c| {
            c.is_ascii_uppercase() || c.is_ascii_digit()
        }),
        ("npm_", 30, false, |c| c.is_ascii_alphanumeric()),
        ("sk_live_", 24, false, |c| c.is_ascii_alphanumeric()),
        ("rk_live_", 24, false, |c| c.is_ascii_alphanumeric()),
        ("sk_test_", 24, false, |c| c.is_ascii_alphanumeric()),
        ("rk_test_", 24, false, |c| c.is_ascii_alphanumeric()),
    ];
    SHAPES.iter().any(|(prefix, len, exact, allowed)| {
        text.match_indices(prefix).any(|(i, _)| {
            // The boundary before the scan, as in `shaped` (Terra #18248).
            if joins_before(&text[..i]) {
                return false;
            }
            let rest = &text[i + prefix.len()..];
            let n = rest.chars().take_while(|&c| allowed(c)).count();
            let after = rest.chars().nth(n);
            (if *exact { n == *len } else { n >= *len })
                && after.is_none_or(|c| !c.is_alphanumeric() && !combining(c))
        })
    })
}

/// A `password` or `token` assignment with a long value (edd-01a18867,
/// Secret scan): a key ending in `password`, `passwd` or `token` (any
/// case: `DB_PASSWORD`, `apiToken`, `"token"`), then `=` or `:` (spaces
/// around it allowed, `::` and `==` aren't assignments), then a value of
/// at least 20 characters with both a letter and a digit in it, and not a
/// UUID (an id, not a secret). A quoted value runs to its matching quote
/// and may hold any printable character, unless it starts a placeholder
/// (`"$VAR"`, `"<pw>"`, `"{pw}"`, `"%s"`). An unquoted one uses the
/// alphabet tokens and base64 use (no `.`): one that starts a placeholder
/// stops before 20, and one that runs on into an expression (`env.X`,
/// `svc.get_token()`, a call) isn't one, however long its first word.
fn has_assignment(text: &str) -> bool {
    const MIN: usize = 20;
    let key_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    let value_char =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '+' | '=' | '~');
    text.match_indices(['=', ':']).any(|(i, sep)| {
        let after = &text[i + 1..];
        let before = &text[..i];
        // `::` (a path), `==`, `!=`, `<=`, `>=` and `:=`'s colon aren't
        // the assignment itself.
        if after.starts_with(['=', ':'])
            || before.ends_with([':', '=', '!', '<', '>'])
            || (sep == ":" && after.starts_with("//"))
        {
            return false;
        }
        let key_end = before.trim_end_matches([' ', '\t', '"', '\'']);
        let key_start = key_end
            .char_indices()
            .rev()
            .take_while(|&(_, c)| key_char(c))
            .last()
            .map_or(key_end.len(), |(at, _)| at);
        let key = key_end[key_start..].to_ascii_lowercase();
        if !["password", "passwd", "token"]
            .iter()
            .any(|k| key.ends_with(k))
        {
            return false;
        }
        let long = |value: &str| {
            value.len() >= MIN
                && value.bytes().any(|b| b.is_ascii_digit())
                && value.bytes().any(|b| b.is_ascii_alphabetic())
                && !is_uuid(value)
        };
        let value = after.trim_start_matches([' ', '\t']);
        // A quoted value runs to its matching quote, whatever printable
        // characters it holds (DECISION 15, Terra #18848: a `.` inside
        // quotes is part of the value); a placeholder (`$X`, `<x>`, `{x}`,
        // `%x`) isn't one.
        if let Some(q) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) {
            let body = &value[1..];
            let n = body
                .find(|c: char| c == q || !c.is_ascii_graphic())
                .unwrap_or(body.len());
            let (inner, rest) = body.split_at(n);
            return (rest.is_empty() || rest.starts_with(q))
                && !inner.starts_with(['$', '<', '{', '%'])
                && long(inner);
        }
        let end = value
            .char_indices()
            .find(|&(_, c)| !value_char(c))
            .map_or(value.len(), |(at, _)| at);
        let (value, rest) = value.split_at(end);
        // The value ends where the assignment does: at the text's end,
        // a space, a quote or a delimiter, or a full stop that ends a
        // sentence. Anything else (`.get_token()`, `(`) continues an
        // expression, and its prefix isn't a value (Terra #18736).
        let mut next = rest.chars();
        let ends = match next.next() {
            None => true,
            Some('.') => next.next().is_none_or(char::is_whitespace),
            Some(c) => {
                c.is_whitespace()
                    || matches!(
                        c,
                        '"' | '\'' | '`' | ',' | ';' | '&' | ')' | ']' | '}' | '#' | '\\'
                    )
            }
        };
        ends && long(value)
    })
}

/// Whether structured data (a message's parts, a JSON object) holds a
/// credential: any string or key [`looks_secret`] flags, or an object
/// member whose string value, written as `key=value`, is a `password` or
/// `token` assignment. Scanning keys and values apart would miss
/// `{"token": "<value>"}` (Terra #18736).
pub(crate) fn value_looks_secret(v: &serde_json::Value) -> bool {
    use serde_json::Value;
    match v {
        Value::String(s) => looks_secret(s),
        Value::Array(a) => a.iter().any(value_looks_secret),
        Value::Object(o) => o.iter().any(|(k, x)| {
            looks_secret(k)
                || value_looks_secret(x)
                // One reader for every assignment (DECISION 15): the joined
                // pair goes through every layer `looks_secret` reads.
                || x.as_str()
                    .is_some_and(|x| looks_secret(&format!("{k}={x}")))
        }),
        _ => false,
    }
}

/// A JWT: three dot-separated base64url parts of at least 10 characters,
/// the first a JSON header (`eyJ`). One pass over the text's words.
fn has_jwt(text: &str) -> bool {
    text.split(|c: char| !(token_char(c) || c == '.'))
        .any(|word| {
            let parts: Vec<&str> = word.split('.').collect();
            parts
                .windows(3)
                .any(|w| w[0].starts_with("eyJ") && w.iter().all(|p| p.len() >= 10))
        })
}

/// A URL with a password in it: `scheme://user:password@host` (the user
/// may be empty, `@` written either way). Placeholders (`$VAR`, `<pw>`,
/// `{pw}`) and a password that only repeats the user (`guest:guest`, a
/// local default) aren't credentials.
fn has_url_password(text: &str, urls: Urls<'_>) -> bool {
    // What a URL's user and password can hold, unencoded (RFC 3986).
    let userinfo = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '-' | '.' | '_' | '~' | '!' | '$' | '&' | '\'' | '(' | ')' | '*' | '+'
            )
            || matches!(c, ',' | ';' | '=' | ':' | '%' | '@' | '\u{FF20}')
    };
    // `host:8080,` or `host:8000'`: a port that prose or code goes on
    // from, not a password.
    // Only after a host (`localhost`, a dotted name or address).
    let port_then_text = |user: &str, p: &str| {
        let host = user == "localhost" || user.contains('.');
        let digits = p.chars().take_while(char::is_ascii_digit).count();
        host && digits > 0 && matches!(p[digits..].chars().next(), Some(',' | ';' | '\''))
    };
    // A variable, not a password: `$NAME` (an environment variable), or
    // `$name` when the user is empty or a variable too, as code templates
    // write them (PHP, Perl, shell). `$hunter2` after a plain user, or
    // `$Zx9k…`, may well be a password.
    let ident = |r: &str| {
        r.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && r.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let variable = |user: &str, p: &str| {
        p.strip_prefix('$').is_some_and(|r| {
            let upper = ident(r) && r.chars().all(|c| !c.is_ascii_lowercase());
            let lower = ident(r)
                && r.chars().all(|c| !c.is_ascii_uppercase())
                && (user.is_empty() || user.strip_prefix('$').is_some_and(ident));
            upper || lower
        })
    };
    // A mask or a stand-in, as docs and log redaction write them.
    let stand_in = |p: &str| {
        p.chars().all(|c| c == '*')
            || p.chars().all(|c| c == 'x' || c == 'X')
            || [
                "password", "pass", "passwd", "pwd", "secret", "redacted", "changeme",
            ]
            .contains(&p.to_ascii_lowercase().as_str())
    };
    let read = |i: usize| match urls {
        Urls::All => true,
        Urls::None => false,
        Urls::At(at) => at.contains(&i),
    };
    text.match_indices("://")
        .filter(|(i, _)| read(*i))
        .any(|(i, _)| {
            let authority: String = text[i + 3..].chars().take_while(|&c| userinfo(c)).collect();
            let Some(at) = authority.rfind(['@', '\u{FF20}']) else {
                return false;
            };
            authority[..at]
                .split_once(':')
                .is_some_and(|(user, password)| {
                    !password.is_empty()
                        && !variable(user, password)
                        && !stand_in(password)
                        && !port_then_text(user, password)
                        && password != user
                })
        })
}

#[cfg(test)]
mod tests {

    /// A long word of repeated prefixes is checked in linear time: each
    /// match's boundary first, never its whole suffix (Terra #18248).
    #[test]
    fn repeated_prefixes_inside_a_word_dont_scan_quadratically() {
        let started = std::time::Instant::now();
        for unit in ["sk-", "AKIA", "ghp_"] {
            let text = format!("x{}", unit.repeat(100_000));
            assert!(!looks_secret(&text), "{unit}");
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    /// A UUID alone, by length and positions; nothing near it (Terra
    /// #18301: the check allocates nothing, a megabyte of dashes included).
    #[test]
    fn is_uuid_is_exact_and_allocation_free() {
        assert!(super::is_uuid("01a1a432-73ed-71c6-8ff2-bbd9b188f03c"));
        assert!(super::is_uuid("01A1A432-73ED-71C6-8FF2-BBD9B188F03C"));
        for not in [
            "01a1a432-73ed-71c6-8ff2-bbd9b188f03",
            "01a1a432-73ed-71c6-8ff2-bbd9b188f03cc",
            "01a1a43273ed-71c6-8ff2-bbd9b188f03c-",
            "01a1a432-73ed-71c6-8ff2_bbd9b188f03c",
            "01a1a432-73ed-71c6-8ff2-bbd9b188f03g",
        ] {
            assert!(!super::is_uuid(not), "{not}");
        }
        assert!(!super::is_uuid(&"-".repeat(1 << 20)));
        assert!(looks_secret(&format!("sk-{}", "-".repeat(1 << 20))));
    }

    /// An id that holds a key prefix inside a word isn't a key: a task id
    /// quoted in a review (`task-01a1…` holds `sk-01a1…`) or a `disk-…`
    /// name; a real-shaped key still is (orchestrator, 2026-11-03: a
    /// review's reply was refused for quoting a task id).
    #[test]
    fn ids_that_hold_a_key_prefix_inside_a_word_pass() {
        for text in [
            "task-01a1a432-73ed-71c6-8ff2-bbd9b188f03c",
            "see task-01a1a432-73ed-71c6-8ff2-bbd9b188f03c, round 2",
            "(task-01a1a432-73ed-71c6-8ff2-bbd9b188f03c)",
            "`task-01a1a432-73ed-71c6-8ff2-bbd9b188f03c`",
            "fix task-01a1a432\u{2026} first",
            "task-01a1a432\ntask-01a1a460-73ed-71c6-8ff2-bbd9b188f03c",
            concat!("disk-", "0123456789abcdefghijklmnop"),
            "risk-assessment-for-the-0-7-2-release-notes",
            "mdkg-next msg thread task-01a1a432-73ed-71c6-8ff2-bbd9b188f03c --json",
            // The id's tail quoted on its own: hex and dashes, no key.
            "it holds 'sk-01a1a432-73ed-71c6-8ff2-bbd9b188f03c'",
            // A dash or underscore doesn't start a token.
            concat!("my-sk-", "AbCdEfGhIjKlMnOpQrStUvWx"),
            concat!("x_sk_org_", "AbCdEfGhIjKlMnOpQrStUvWxYz"),
        ] {
            assert!(!looks_secret(text), "{text}");
        }
        for key in [
            concat!("sk-", "proj-", "AbCdEfGhIjKlMnOpQrStUvWxYz012345"),
            concat!("key: sk-", "AbCdEfGhIjKlMnOpQrStUvWx"),
            concat!("(sk-", "ant-", "AbCdEfGhIjKlMnOpQrStUvWx)"),
            concat!("'sk-", "0123456789abcdef0123456789abcdeG'"),
            // Unhyphenated hex is a key shape some services use; only a
            // UUID alone is an id (Terra #18248).
            concat!("sk-", "0123456789abcdef0123456789abcdef"),
            concat!("sk-", "01a1a432-73ed-71c6-8ff2-bbd9b188f03c-AbCdEf"),
            // A dash or underscore that starts a word is still a boundary:
            // Markdown emphasis, a list dash (Terra #18248).
            concat!("see _ghp_", "AbCdEfGhIjKlMnOpQrStUvWxYz_ now"),
            concat!("key: -sk-", "AbCdEfGhIjKlMnOpQrStUvWx"),
            concat!("__sk-", "proj-AbCdEfGhIjKlMnOpQrStUvWx"),
            concat!("sk_org_", "AbCdEfGhIjKlMnOpQrStUvWxYz"),
        ] {
            assert!(looks_secret(key), "{key}");
        }
    }
    use super::looks_secret;

    #[test]
    fn shared_export_credential_cases_are_refused() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/credential-cases.json");
        let cases: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("shared credential fragments"),
        )
        .expect("credential fixture JSON");
        assert_eq!(cases["version"], 1);
        for parts in cases["credentials"].as_array().expect("credentials") {
            let text: String = parts
                .as_array()
                .expect("fragments")
                .iter()
                .map(|p| p.as_str().expect("fragment"))
                .collect();
            assert!(
                looks_secret(&text),
                "missed shared credential prefix {:?}",
                parts[0]
            );
            assert!(
                looks_secret(&format!("literal\\n{text}")),
                "escaped prefix hid credential {:?}",
                parts[0]
            );
            // Every spelling of a line break or space separates words,
            // URL-encoded once or more (e2e B10).
            for sep in cases["separators"].as_array().expect("separators") {
                let sep = sep.as_str().expect("separator");
                assert!(
                    looks_secret(&format!("literal{sep}{text}")),
                    "separator {sep:?} hid credential {:?}",
                    parts[0]
                );
            }
        }
        for parts in cases["ordinary"].as_array().expect("ordinary") {
            let text: String = parts
                .as_array()
                .expect("fragments")
                .iter()
                .map(|p| p.as_str().expect("fragment"))
                .collect();
            assert!(!looks_secret(&text), "{text}");
        }
    }

    /// Files whose lines may look like credentials, each with why: the
    /// detectors' own definitions, which name what they look for. Line
    /// markers limit an entry to lines holding one of them; none allows the
    /// whole file. An entry that's tracked but no longer matches fails the
    /// check, so the list can't go stale (orchestrator #12149); one that
    /// isn't tracked (the public export leaves some scripts out) is absent.
    const DETECTOR_FILES: &[(&str, &[&str], &str)] = &[
        (
            "crates/mdkg-cli/src/credentials.rs",
            &["text.contains(\"-----BEGIN\")"],
            "looks_secret's own key-block check names both of its markers",
        ),
        (
            "scripts/leak-scan.py",
            &[],
            "the export scanner's patterns and self-test",
        ),
        (
            "scripts/ci-remote.sh",
            &[],
            "CI's leak patterns and their self-test",
        ),
        (
            "scripts/export-public.sh",
            &[],
            "the public export's pattern lists",
        ),
        // Pending: fixtures in files other lanes held when these were split
        // (8cf3c619's review); the orchestrator routes them. Each is pinned
        // to its own lines, so nothing else in those files passes (review
        // of 630ac1da). Remove each when it's split: the check fails until
        // it's gone.
        (
            "crates/mdkg-cli/tests/msg.rs",
            &[
                "send(\"note\", \"token ",
                "\"here it is:\\n",
                "fs::write(&file, \"\\t",
            ],
            "pending: three unsplit fixtures, held by f-mail",
        ),
        (
            "crates/mdkg-run/tests/fixtures/fake-codex.sh",
            &["printf 'SECRET=1\\n"],
            "pending: an unsplit fixture, held by f-wake",
        ),
    ];

    /// No tracked line matches on its own (review of 8cf3c619): a fixture
    /// that did would trip `secret.scan` whenever a commit re-adds it, and
    /// on a push that scans history. Fixtures are split with `concat!`;
    /// the detectors' definitions are listed in [`DETECTOR_FILES`]. A match
    /// is reported by path and line, never by its text.
    #[test]
    fn no_tracked_line_looks_like_a_credential_on_its_own() {
        let top = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        // Only a git work tree has tracked files: a packaged crate or a
        // tarball, with no `.git`, has nothing to check (review of
        // 630ac1da). A checkout whose git can't list them fails (review of
        // ef29ad3f), never passing having checked nothing.
        if !top.join(".git").exists() {
            return;
        }
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&top)
            .args(["ls-files", "-z"])
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git ls-files failed in a checkout");
        let mut tracked = std::collections::BTreeSet::new();
        let mut found = Vec::new();
        // Hits per (path, marker), so one marker that stops matching is
        // stale even while its file's others still match (review of
        // ef29ad3f); a whole-file entry is keyed by its path alone.
        let mut listed_hits = std::collections::BTreeSet::new();
        for name in out.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
            let name = String::from_utf8_lossy(name).into_owned();
            tracked.insert(name.clone());
            let Ok(bytes) = std::fs::read(top.join(&name)) else {
                continue;
            };
            if bytes.contains(&0) {
                continue;
            }
            let listed = DETECTOR_FILES.iter().find(|(path, _, _)| *path == name);
            for (n, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                if !looks_secret(line) {
                    continue;
                }
                let hit = listed.and_then(|(path, markers, _)| {
                    if markers.is_empty() {
                        Some((*path, ""))
                    } else {
                        markers
                            .iter()
                            .find(|m| line.contains(*m))
                            .map(|m| (*path, *m))
                    }
                });
                match hit {
                    Some(key) => {
                        listed_hits.insert(key);
                    }
                    None => found.push(format!("{name}:{}", n + 1)),
                }
            }
        }
        assert!(
            found.is_empty(),
            "lines that look like credentials: {found:?}"
        );
        let stale: Vec<String> = DETECTOR_FILES
            .iter()
            .filter(|(path, _, _)| tracked.contains(*path))
            .flat_map(|(path, markers, _)| {
                let keys: Vec<&str> = if markers.is_empty() {
                    vec![""]
                } else {
                    markers.to_vec()
                };
                keys.into_iter()
                    .filter(|m| !listed_hits.contains(&(*path, *m)))
                    .map(|m| format!("{path} {m:?}"))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(
            stale.is_empty(),
            "listed files that no longer match: {stale:?}"
        );
    }

    #[test]
    fn password_and_token_assignments_with_long_values_are_secrets() {
        for secret in [
            concat!("password=Zx9kq2LmQ7", "vB4nT8rW1y"),
            concat!("DB_PASSWORD = \"Zx9kq2LmQ7", "vB4nT8rW1y\""),
            concat!("export API_TOKEN='a1b2c3d4e5", "f6g7h8i9j0k1'"),
            concat!("{\"token\": \"a1b2c3d4e5", "f6g7h8i9j0k1\"}"),
            concat!("apiToken: a1b2c3d4e5f6g7h8i9", "j0k1"),
            concat!("passwd=YWJjZGVmZ2hpamts", "bW5vcA1=="),
            concat!("note\nauth_token=Zx9kq2LmQ7", "vB4nT8rW1y"),
        ] {
            assert!(looks_secret(secret), "{secret}");
        }
        for ordinary in [
            "password=hunter2",
            "token=abcdefghijklmnopqrstuvwxyz",
            "password=$DB_PASSWORD_FROM_THE_ENV_1",
            "token=${{ secrets.GITHUB_TOKEN_2 }}",
            "password=<your password here 1234567>",
            "token=env.MDKG_TOKEN_VALUE_FOR_2026()",
            "token=01a1a597-d6bb-7652-a7ac-cf4b4d476e45",
            "if token == expected_token_value_12345 {",
            "let token = concat!(\"a1b2c3d4e5\", \"f6\");",
            "mdkg::token::parse_the_value_2026_x",
            "the token: see docs",
            "tokens=a1b2c3d4e5f6g7h8i9j0k1",
            "add the long `password=`/`token=` patterns",
            "token = auth_provider_service_v2.get_token()",
            "password=read_password_from_vault_2026(name)",
        ] {
            assert!(!looks_secret(ordinary), "{ordinary}");
        }
        // DECISION 15 (Terra #18848): a quoted value runs to its quote,
        // punctuation included; placeholders and UUIDs in quotes aren't.
        assert!(looks_secret(concat!(
            "password=\"alpha01234",
            "56789ABCDE.extra\""
        )));
        for ordinary in [
            "token=\"${{ secrets.GITHUB_TOKEN_2 }}\"",
            "token=\"<your token here 1234567>\"",
            "password=\"$DB_PASSWORD_FROM_THE_ENV_1\"",
            "token='01a1a597-d6bb-7652-a7ac-cf4b4d476e45'",
            "token=\"{{ .Values.apiToken2026 }}\"",
        ] {
            assert!(!looks_secret(ordinary), "{ordinary}");
        }
        // A value that ends a sentence still counts.
        assert!(looks_secret(concat!(
            "the token=a1b2c3d4e5",
            "f6g7h8i9j0k1. Then"
        )));
        // Structured data keeps a member's key with its value.
        let value = concat!("a1b2c3d4e5", "f6g7h8i9j0k1");
        // A joined pair is read through every decoding layer (DECISION 15).
        assert!(super::value_looks_secret(&serde_json::json!({
            "token": concat!("alpha%2F", "0123456789ABCDE")
        })));
        for data in [
            serde_json::json!({"token": value}),
            serde_json::json!([{"kind": "data", "data": {"apiToken": value}}]),
        ] {
            assert!(super::value_looks_secret(&data), "{data}");
        }
        for data in [
            serde_json::json!({"tokens": value}),
            serde_json::json!({"token": "01a1a597-d6bb-7652-a7ac-cf4b4d476e45"}),
            serde_json::json!({"note": value}),
        ] {
            assert!(!super::value_looks_secret(&data), "{data}");
        }
    }

    #[test]
    fn secrets_are_recognised_without_tripping_on_ordinary_text() {
        assert!(looks_secret(concat!(
            "-----BEGIN OPENSSH PRIV",
            "ATE KEY-----\nabc\n-----END OPENSSH PRIV",
            "ATE KEY-----"
        )));
        assert!(looks_secret(concat!(
            "token: ghp_",
            "abcdefghijklmnopqrstuvwxyz0123456789"
        )));
        assert!(looks_secret(concat!("key=AKIA", "IOSFODNN7EXAMPLE")));
        // Task-36: more prefixes, JWTs and URLs with a password.
        for secret in [
            concat!("ASIA", "IOSFODNN7EXAMPLE"),
            concat!("sk_", "live_", "abcdefghijklmnopqrstuvwx"),
            concat!("npm_", "abcdefghijklmnopqrstuvwxyz0123456789"),
            concat!(
                "eyJhbGciOiJIUzI1NiJ9.",
                "eyJzdWIiOiIxMjM0In0.abcdefghijklmnop"
            ),
            concat!("DATABASE_URL=postgres://app:hunter2", "@db.internal:5432/x"),
            "postgres://app:hunter2\u{FF20}db.internal",
        ] {
            assert!(looks_secret(secret), "{secret}");
        }
        assert!(looks_secret(concat!("redis://:hunter2", "@cache:6379")));
        assert!(looks_secret(&format!("x {}", ".eyJabcdefghij".repeat(3))));
        // Review round 2 of audit-core.
        for secret in [
            concat!("key_AKIA", "IOSFODNN7EXAMPLE"),
            concat!("AKIA", "IOSFODNN7EXAMPLE_backup"),
            concat!("postgres://app:$Zx9kq2LmQ7", "@db.prod"),
            concat!("sk_", "live_", "abcdefghijklmnopqrstuvwx"),
        ] {
            assert!(looks_secret(secret), "{secret}");
        }
        for ordinary in [
            "postgresql://scott:***@db/x",
            "amqp://guest:**@localhost:5672//",
            "redis://:****@cache",
            "postgres://user:password@localhost/x",
            "USER:PASSWORD@HOST via mysql://USER:PASSWORD@HOST/db",
            "postgres://app:REDACTED@db",
            "postgres://app:xxxxx@db",
            "{'url':'http://localhost:8000','user':'bot@example.com'}",
            "see http://localhost:8080,ops@example.com",
            "is_sk_live_mode_enabled_for_this_account",
        ] {
            assert!(!looks_secret(ordinary), "{ordinary}");
        }
        // Review round 4 of audit-core.
        for secret in [
            concat!("postgres://app:$ecretsauce", "@db.prod"),
            concat!("postgres://app:$hunter2", "@db.prod"),
            concat!("postgres://app:2024;Spring!", "@db.prod"),
            concat!("redis://:1;Zx9kq2Lm", "@cache"),
        ] {
            assert!(looks_secret(secret), "{secret}");
        }
        assert!(!looks_secret("see http://example.com:8080;admin@corp"));
        // Review round 3 of audit-core.
        for secret in [
            concat!("postgres://app:pa;ss9Zk2", "@db.prod"),
            concat!("postgres://app:Zx9,kq2Lm", "@db.prod"),
            concat!("redis://:a1b2;c3d4", "@cache"),
        ] {
            assert!(looks_secret(secret), "{secret}");
        }
        for ordinary in [
            r#"$dsn = "mysql://$user:$pass@$host/$db";"#,
            "DATABASE_URL=postgres://$db_user:$db_password@$db_host:5432/app",
            "redis://:$redis_pass@cache:6379",
        ] {
            assert!(!looks_secret(ordinary), "{ordinary}");
        }
        // Review round 1 of audit-core: ordinary text stays sendable.
        for ordinary in [
            "ssh://git@github.com/x/y",
            "https://example.com/a:b@c",
            "see eyJ for the header",
            "http://user:@host",
            "AWS_REGION_ASIA_PACIFIC_SOUTHEAST_1",
            "ASIA_PACIFIC_NORTHEAST",
            "npm_config_strict_peer_dependencies=false",
            "npm_package_dependencies_typescript_version",
            "postgres://postgres:postgres@localhost:5432/app",
            "postgres://$POSTGRES_USER:$POSTGRES_PASSWORD@db:5432/app",
            "mongodb://<username>:<password>@cluster0.example.net",
            "amqp://guest:guest@localhost:5672/",
            r#"{"url":"https://example.com","user":"bot@example.com"}"#,
        ] {
            assert!(!looks_secret(ordinary), "{ordinary}");
        }
        // Friction 40: an escaped newline, return or tab is a boundary, for
        // every shape; the letter after the backslash isn't part of a word.
        for escape in ["\\n", "\\r", "\\t"] {
            for secret in [
                concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789"),
                concat!("AKIA", "IOSFODNN7EXAMPLE"),
                concat!("sk_", "live_", "abcdefghijklmnopqrstuvwx"),
                concat!(
                    "eyJhbGciOiJIUzI1NiJ9.",
                    "eyJzdWIiOiIxMjM0In0.abcdefghijklmnop"
                ),
            ] {
                let text = format!("literal{escape}{secret}");
                assert!(looks_secret(&text), "{text}");
            }
            for ordinary in [
                "is_sk_live_mode_enabled_for_this_account",
                "AWS_REGION_ASIA_PACIFIC_SOUTHEAST_1",
                "npm_config_strict_peer_dependencies=false",
                "the ghp_ prefix marks GitHub tokens",
            ] {
                let text = format!("literal{escape}{ordinary}");
                assert!(!looks_secret(&text), "{text}");
            }
        }
        // Review of c50df765: the other spellings of a control or space.
        for escape in [
            "\\f", "\\v", "\\b", "\\0", "\\012", "\\040", "\\x0a", "\\u000a", "\\u0020", "%0A",
            "%0d", "%09", "%20",
        ] {
            let secret = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");
            let text = format!("literal{escape}{secret}");
            assert!(looks_secret(&text), "{text}");
            let text = format!("literal{escape}is_sk_live_mode_enabled_for_this_account");
            assert!(!looks_secret(&text), "{text}");
        }
        // An escape of a printable character is no boundary, and a
        // multi-byte character beside one survives the second pass; nor
        // is one URL-encoded again, or a wide escape of a letter.
        for escape in [
            "\\x41", "\\u0041", "%41", "%zz", "\\q", "\\060", "\\041", "%2541", "%5Cx41",
            "%5Cu00e9", "\\u00e9", "%C3%A9",
        ] {
            let text = format!(
                "literal{escape}{}",
                concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789")
            );
            assert!(!looks_secret(&text), "{text}");
        }
        assert!(looks_secret(concat!(
            "é\\nghp_",
            "abcdefghijklmnopqrstuvwxyz0123456789 ü"
        )));
        // Any letter right before the prefix, ASCII or not, makes one word
        // (review of 8cf3c619: an id like `tásk-…` isn't a key).
        for word in [
            concat!("tásk-", "0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b"),
            concat!("éghp_", "abcdefghijklmnopqrstuvwxyz0123456789"),
            concat!("ÄAKIA", "IOSFODNN7EXAMPLE"),
            concat!("AKIA", "IOSFODNN7EXAMPLEé"),
        ] {
            assert!(!looks_secret(word), "{word}");
        }
        assert!(looks_secret(concat!(
            "é ghp_",
            "abcdefghijklmnopqrstuvwxyz0123456789"
        )));
        // Decomposed (NFD) text joins the same way: a combining mark is
        // part of the letter before it (review of c6df840d).
        for word in [
            concat!("ta\u{301}sk-", "0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b"),
            concat!("e\u{301}ghp_", "abcdefghijklmnopqrstuvwxyz0123456789"),
            concat!("AKIA", "IOSFODNN7EXAMPLE\u{301}"),
        ] {
            assert!(!looks_secret(word), "{word:?}");
        }
        // A mark with no letter before it joins nothing.
        assert!(looks_secret(concat!(
            " \u{301}ghp_",
            "abcdefghijklmnopqrstuvwxyz0123456789"
        )));
        // A letter right before the prefix is still a word, not a boundary.
        assert!(!looks_secret(concat!(
            "literalnghp_",
            "abcdefghijklmnopqrstuvwxyz0123456789"
        )));
        assert!(!looks_secret("the ghp_ prefix marks GitHub tokens"));
        // An encoded colon in a URL's user name delimits nothing, so the
        // decoded layer isn't a password (review of c0130d38); a real one
        // still is.
        assert!(!looks_secret("see https://user%3Aname@example.com/repo"));
        assert!(!looks_secret("see https://user%253Aname@example.com/repo"));
        assert!(looks_secret(concat!(
            "postgres://app:",
            "hunter2@db.internal"
        )));
        // A URL encoded whole is still one (review of bf42c75e).
        assert!(looks_secret(concat!(
            "dsn=postgres%3A%2F%2Fapp%3A",
            "hunter2%40db.internal"
        )));
        assert!(looks_secret(concat!(
            "dsn=postgres%253A%252F%252Fapp%253A",
            "hunter2%2540db.internal"
        )));
        // Another URL in the text doesn't stop an encoded one being read
        // (review of 154d5016).
        assert!(looks_secret(concat!(
            "docs at https://example.com/a; dsn=postgres%3A%2F%2Fapp%3A",
            "hunter2%40db.internal"
        )));
        // A URL encoded whole whose user writes an encoded colon has no
        // password: only the layer that revealed it is read (review of
        // 154d5016).
        assert!(!looks_secret(
            "dsn=postgres%3A%2F%2Fuser%253Aname%40db.internal"
        ));
        assert!(!looks_secret("secret = os.environ['X']"));
        assert!(!looks_secret(concat!(
            "MASKIA",
            "IOSFODNN7EXAMPLEX is not an AWS key"
        )));
    }
}
